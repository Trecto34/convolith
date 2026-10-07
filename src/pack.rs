//! Deterministic portable container for complete canonical archives.
use anyhow::{bail, Context, Result};
use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct FileRef {
    path: String,
    size: u64,
    sha256: String,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    tool_version: String,
    source_manifest_id: String,
    files: Vec<FileRef>,
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for e in walkdir::WalkDir::new(root).follow_links(false) {
        let e = e?;
        if e.file_type().is_symlink() {
            bail!(
                "archive contains a symlink, refusing to omit it: {}",
                e.path().display()
            );
        }
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path().strip_prefix(root)?;
        let rel = p.to_string_lossy().replace('\\', "/");
        if rel.starts_with("staging/")
            || rel == "indexes/search.sqlite"
            || rel.starts_with("indexes/search.sqlite-")
            || rel == "provenance/provenance.sqlite-wal"
            || rel == "provenance/provenance.sqlite-shm"
        {
            continue;
        }
        out.insert(rel, fs::read(e.path())?);
    }
    Ok(out)
}
fn verify_checksum_map(map: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let text = std::str::from_utf8(
        map.get("checksums.sha256")
            .context("checksums.sha256 missing")?,
    )?;
    let mut seen = BTreeSet::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (want, rel) = line
            .split_once("  ")
            .with_context(|| format!("malformed checksums.sha256 line {}", i + 1))?;
        let rel = crate::archive::sanitize_entry_path(rel, 4096)
            .context("unsafe checksums.sha256 path")?;
        let name = rel.to_string_lossy().replace('\\', "/");
        if !seen.insert(name.clone()) {
            bail!("duplicate checksums.sha256 entry: {name}");
        }
        let data = map
            .get(&name)
            .with_context(|| format!("checksummed file missing: {name}"))?;
        if sha(data) != want.trim() {
            bail!("archive checksum mismatch: {name}");
        }
    }
    if seen.is_empty() {
        bail!("checksums.sha256 is empty");
    }
    Ok(())
}
fn sqlite_snapshot(root: &Path) -> Result<Vec<u8>> {
    let src_path = root.join("provenance/provenance.sqlite");
    let wal_path = PathBuf::from(format!("{}-wal", src_path.display()));
    if fs::metadata(&wal_path)
        .map(|m| m.len() == 0)
        .unwrap_or(true)
    {
        return Ok(fs::read(src_path)?);
    }
    let src = Connection::open_with_flags(&src_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "convolith-snapshot-{}-{nonce}.sqlite",
        std::process::id()
    ));
    let mut dst = Connection::open(&tmp)?;
    Backup::new(&src, &mut dst)?.run_to_completion(
        64,
        std::time::Duration::from_millis(10),
        None,
    )?;
    drop(dst);
    let bytes = fs::read(&tmp)?;
    fs::remove_file(tmp)?;
    Ok(bytes)
}
pub fn pack(root: &Path, output: &Path, force: bool) -> Result<()> {
    if output.exists() && !force {
        bail!("output exists (use --force): {}", output.display())
    }
    let mut fsmap = files(root)?;
    verify_checksum_map(&fsmap)?;
    // Collection resume state is included in source checksum verification, but
    // is runtime state rather than canonical archive content.
    fsmap.remove("collect-state.json");
    let snapshot = sqlite_snapshot(root)?;
    fsmap.insert("provenance/provenance.sqlite".into(), snapshot.clone());
    // Ensure the restored snapshot's checksum reflects committed WAL pages.
    let old = std::str::from_utf8(fsmap.get("checksums.sha256").unwrap())?;
    let revised = old
        .lines()
        .filter_map(|line| {
            if line.ends_with("  collect-state.json") {
                return None;
            }
            Some(match line.split_once("  ") {
                Some((_, "provenance/provenance.sqlite")) => {
                    format!("{}  provenance/provenance.sqlite", sha(&snapshot))
                }
                _ => line.to_owned(),
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fsmap.insert("checksums.sha256".into(), revised.into_bytes());
    let raw = fsmap
        .get("manifest.json")
        .context("archive lacks manifest.json")?;
    let mid = sha(raw);
    let refs = fsmap
        .iter()
        .map(|(p, b)| FileRef {
            path: p.clone(),
            size: b.len() as u64,
            sha256: sha(b),
        })
        .collect();
    let m = Manifest {
        format: "convolith-pack".into(),
        version: 1,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        source_manifest_id: mid,
        files: refs,
    };
    fsmap.insert("PACK-MANIFEST.json".into(), serde_json::to_vec(&m)?);
    let parent = output.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = output.with_extension(format!("{}.tmp", std::process::id()));
    let f = fs::File::create(&tmp)?;
    let enc = zstd::Encoder::new(f, 9)?;
    let mut tar = tar::Builder::new(enc);
    for (name, data) in fsmap {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_cksum();
        tar.append_data(&mut h, name, &data[..])?;
    }
    let enc = tar.into_inner()?;
    let mut f = enc.finish()?;
    f.flush()?;
    drop(f);
    if output.exists() && force {
        if !output.is_file() {
            bail!("output exists and is not a file: {}", output.display());
        }
        // POSIX rename replaces an existing file atomically. Windows requires
        // removing the destination first because std does not expose replace.
        #[cfg(windows)]
        fs::remove_file(output)?;
    }
    fs::rename(&tmp, output).context("publishing pack")?;
    Ok(())
}
pub fn unpack(pack: &Path, output: &Path, force: bool) -> Result<()> {
    if output.exists() && !output.is_dir() {
        bail!("destination exists and is not a directory");
    }
    if output.exists() && !force && fs::read_dir(output)?.next().is_some() {
        bail!("destination is not empty (use --force to replace it)")
    }
    let parent = output.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".convolith-unpack-{}", std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir(&tmp)?;
    let result = (|| -> Result<()> {
        let f = fs::File::open(pack)?;
        let dec = zstd::Decoder::new(f)?;
        let mut ar = tar::Archive::new(dec);
        let mut seen = BTreeMap::<String, Vec<u8>>::new();
        let mut total = 0u64;
        let mut count = 0usize;
        for entry in ar.entries()? {
            let mut e = entry?;
            count += 1;
            if count > 100_000 {
                bail!("pack exceeds entry-count limit (100000)");
            }
            let ty = e.header().entry_type();
            if !ty.is_file() {
                bail!("unsupported tar entry type")
            };
            let raw = e.path()?.to_string_lossy().to_string();
            let rel =
                crate::archive::sanitize_entry_path(&raw, 4096).context("unsafe pack path")?;
            let name = rel.to_string_lossy().replace('\\', "/");
            if seen.contains_key(&name) {
                bail!("duplicate pack entry: {name}")
            };
            let size = e.size();
            if size > 2 * 1024 * 1024 * 1024 {
                bail!("pack entry exceeds size limit")
            };
            total = total
                .checked_add(size)
                .context("decompressed size overflow")?;
            if total > 20 * 1024 * 1024 * 1024 {
                bail!("pack exceeds aggregate decompressed-size limit (20 GiB)");
            }
            let mut b = Vec::new();
            e.read_to_end(&mut b)?;
            if b.len() as u64 != size {
                bail!("truncated entry")
            };
            seen.insert(name, b);
        }
        let mb = seen
            .remove("PACK-MANIFEST.json")
            .context("pack manifest missing")?;
        let m: Manifest = serde_json::from_slice(&mb).context("invalid pack manifest")?;
        if m.format != "convolith-pack" || m.version != 1 {
            bail!("unsupported pack format")
        };
        if seen.len() != m.files.len() {
            bail!("pack file list does not match entries")
        };
        for r in &m.files {
            crate::archive::sanitize_entry_path(&r.path, 4096)
                .context("unsafe manifest file path")?;
            let b = seen.get(&r.path).context("listed file missing")?;
            if b.len() as u64 != r.size || sha(b) != r.sha256 {
                bail!("pack file integrity failure: {}", r.path)
            }
        }
        if sha(seen
            .get("manifest.json")
            .context("dataset manifest missing")?)
            != m.source_manifest_id
        {
            bail!("source manifest id mismatch")
        };
        verify_checksum_map(&seen)?;
        for (name, b) in &seen {
            let dest = tmp.join(PathBuf::from(name));
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?
            };
            let mut f = fs::File::create(dest)?;
            f.write_all(b)?;
        }
        if output.exists() {
            if output.is_dir() {
                fs::remove_dir_all(output)?;
            } else {
                bail!("destination exists and is not a directory");
            }
        }
        fs::rename(&tmp, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}
