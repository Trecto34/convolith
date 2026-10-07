use sha2::Digest;
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::Command,
};

fn temp(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-pack-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}
fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_convolith"))
        .args(args)
        .output()
        .unwrap()
}
fn archive(base: &Path) -> PathBuf {
    let input = base.join("input.jsonl");
    fs::write(&input, "{\"role\":\"user\",\"content\":\"synthetic fixture alpha\"}\n{\"role\":\"assistant\",\"content\":\"synthetic fixture beta\"}\n").unwrap();
    let out = base.join("archive");
    let o = cli(&[
        "import",
        input.to_str().unwrap(),
        "--output",
        out.to_str().unwrap(),
    ]);
    assert!(
        o.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    out
}
fn event_ids(dir: &Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for e in walkdir::WalkDir::new(dir.join("data/events"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let f = fs::File::open(e.path()).unwrap();
        let d = zstd::Decoder::new(f).unwrap();
        for line in std::io::BufReader::new(d).lines().map_while(Result::ok) {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            ids.insert(v["event_id"].as_str().unwrap().to_owned());
        }
    }
    ids
}
fn listed_hashes(dir: &Path) -> String {
    fs::read_to_string(dir.join("checksums.sha256")).unwrap()
}
fn rewrite_pack(src: &Path, dst: &Path, change: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) {
    let d = zstd::Decoder::new(fs::File::open(src).unwrap()).unwrap();
    let mut ar = tar::Archive::new(d);
    let mut all = Vec::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        let n = e.path().unwrap().to_string_lossy().to_string();
        let mut b = Vec::new();
        e.read_to_end(&mut b).unwrap();
        all.push((n, b));
    }
    change(&mut all);
    let out = fs::File::create(dst).unwrap();
    let enc = zstd::Encoder::new(out, 9).unwrap();
    let mut tar = tar::Builder::new(enc);
    for (n, b) in all {
        let mut h = tar::Header::new_gnu();
        h.set_size(b.len() as u64);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_cksum();
        if n == "../escape" {
            let raw = h.as_mut_bytes();
            raw[..100].fill(0);
            raw[..9].copy_from_slice(b"../escape");
            h.set_cksum();
            tar.append(&h, b.as_slice()).unwrap();
        } else {
            tar.append_data(&mut h, n, b.as_slice()).unwrap();
        }
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn pack_roundtrip_is_deterministic_and_validates() {
    let t = temp("roundtrip");
    let src = archive(&t);
    let before = listed_hashes(&src);
    let p1 = t.join("one.convolith");
    let p2 = t.join("two.convolith");
    assert!(cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        p1.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(before, listed_hashes(&src));
    assert!(cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        p2.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(fs::read(&p1).unwrap(), fs::read(&p2).unwrap());
    let restored = t.join("restored");
    assert!(cli(&[
        "unpack",
        p1.to_str().unwrap(),
        "--output",
        restored.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(event_ids(&src), event_ids(&restored));
    let shard_names = |d: &Path| {
        walkdir::WalkDir::new(d.join("data/events"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().to_string(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(shard_names(&src), shard_names(&restored));
    assert!(convolith::validate::validate(&restored).unwrap().passed());
    assert!(convolith::validate::validate(&restored).unwrap().passed());
    let a = convolith::dataset::Layout { root: src.clone() };
    let b = convolith::dataset::Layout {
        root: restored.clone(),
    };
    let counts = |l: &convolith::dataset::Layout| {
        let c = rusqlite::Connection::open(l.provenance_db()).unwrap();
        (
            c.query_row("select count(*) from source", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            c.query_row("select count(*) from observation", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        )
    };
    assert_eq!(counts(&a), counts(&b));
    let id = event_ids(&src).into_iter().next().unwrap();
    assert!(convolith::report::inspect_event(&restored, &id).is_ok());
    assert!(convolith::report::provenance(&restored, &id).is_ok());
    let p3 = t.join("three.convolith");
    assert!(cli(&[
        "pack",
        restored.to_str().unwrap(),
        "--output",
        p3.to_str().unwrap()
    ])
    .status
    .success());
    assert_eq!(fs::read(p1).unwrap(), fs::read(p3).unwrap());
}

#[test]
fn pack_rejects_corruption_and_unsafe_entries() {
    let t = temp("tamper");
    let src = archive(&t);
    let good = t.join("good.pack");
    assert!(cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        good.to_str().unwrap()
    ])
    .status
    .success());
    let mut flipped = fs::read(&good).unwrap();
    let at = flipped.len() / 2;
    flipped[at] ^= 0x40;
    let fp = t.join("flip.pack");
    fs::write(&fp, flipped).unwrap();
    let reject = |p: &Path| {
        let o = cli(&[
            "unpack",
            p.to_str().unwrap(),
            "--output",
            t.join("rejected").to_str().unwrap(),
        ]);
        assert!(!o.status.success());
    };
    reject(&fp);
    let bytes = fs::read(&good).unwrap();
    let trunc = t.join("trunc.pack");
    fs::write(&trunc, &bytes[..bytes.len() - 8]).unwrap();
    reject(&trunc);
    for (name, edit) in [("removed", 0u8), ("extra", 1), ("traversal", 2)] {
        let bad = t.join(format!("{name}.pack"));
        rewrite_pack(&good, &bad, |v| match edit {
            0 => {
                let i = v.iter().position(|(n, _)| n == "manifest.json").unwrap();
                v.remove(i);
            }
            1 => v.push(("extra.txt".into(), b"extra".to_vec())),
            _ => v.push(("../escape".into(), b"bad".to_vec())),
        });
        reject(&bad);
    }
    let bad = t.join("tampered-file.pack");
    rewrite_pack(&good, &bad, |v| {
        let (_, b) = v.iter_mut().find(|(n, _)| n == "manifest.json").unwrap();
        b[0] ^= 1;
    });
    reject(&bad);
}

#[test]
fn pack_refuses_corrupt_source_and_existing_outputs() {
    let t = temp("refusal");
    let src = archive(&t);
    let pack = t.join("out.pack");
    fs::write(&pack, b"existing").unwrap();
    assert!(!cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        pack.to_str().unwrap()
    ])
    .status
    .success());
    let target = t.join("not-empty");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), b"x").unwrap();
    let p = t.join("valid.pack");
    assert!(cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        p.to_str().unwrap()
    ])
    .status
    .success());
    assert!(!cli(&[
        "unpack",
        p.to_str().unwrap(),
        "--output",
        target.to_str().unwrap()
    ])
    .status
    .success());
    fs::write(src.join("manifest.json"), b"tampered").unwrap();
    assert!(!cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        t.join("bad.pack").to_str().unwrap()
    ])
    .status
    .success());
}

#[test]
fn packs_consistent_snapshot_with_live_wal() {
    let t = temp("wal");
    let src = archive(&t);
    let db = src.join("provenance/provenance.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS wal_fixture(value TEXT); INSERT INTO wal_fixture VALUES ('committed');").unwrap();
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    assert!(wal.exists());
    let mut checks = String::new();
    for line in listed_hashes(&src).lines() {
        if let Some((_, rel)) = line.split_once("  ") {
            if rel == "provenance/provenance.sqlite" {
                let bytes = fs::read(&db).unwrap();
                checks.push_str(&format!("{:x}  {rel}\n", sha2::Sha256::digest(bytes)));
                continue;
            }
        }
        checks.push_str(line);
        checks.push('\n');
    }
    fs::write(src.join("checksums.sha256"), checks).unwrap();
    let checks_before_pack = listed_hashes(&src);
    // Source checksums cover the main DB; the committed WAL is incorporated in the pack snapshot.
    let p = t.join("wal.pack");
    let o = cli(&[
        "pack",
        src.to_str().unwrap(),
        "--output",
        p.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(checks_before_pack, listed_hashes(&src));
    let restored = t.join("wal-restored");
    assert!(cli(&[
        "unpack",
        p.to_str().unwrap(),
        "--output",
        restored.to_str().unwrap()
    ])
    .status
    .success());
    let c = rusqlite::Connection::open(restored.join("provenance/provenance.sqlite")).unwrap();
    assert_eq!(
        c.query_row("select value from wal_fixture", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "committed"
    );
}
