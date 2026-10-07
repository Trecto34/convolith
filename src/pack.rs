//! Deterministic portable container for complete canonical archives.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path().strip_prefix(root)?;
        let rel = p.to_string_lossy().replace('\\', "/");
        if rel == "collect-state.json"
            || rel.starts_with("staging/")
            || rel == "indexes/search.sqlite"
            || rel.starts_with("indexes/search.sqlite-")
        {
            continue;
        }
        out.insert(rel, fs::read(e.path())?);
    }
    Ok(out)
}
pub fn pack(root: &Path, output: &Path, force: bool) -> Result<()> {
    if output.exists() && !force {
        bail!("output exists (use --force): {}", output.display())
    }
    let mut fsmap = files(root)?;
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
        fs::remove_file(output)?;
    }
    fs::rename(&tmp, output).context("publishing pack")?;
    Ok(())
}
pub fn unpack(pack: &Path, output: &Path, force: bool) -> Result<()> {
    if output.exists() && (!force || fs::read_dir(output)?.next().is_some()) {
        bail!("destination must not exist or must be empty (empty existing directory is accepted only with --force)")
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
        for entry in ar.entries()? {
            let mut e = entry?;
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
        for (name, b) in &seen {
            let dest = tmp.join(PathBuf::from(name));
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?
            };
            let mut f = fs::File::create(dest)?;
            f.write_all(b)?;
        }
        if output.exists() {
            fs::remove_dir(output)?;
        }
        fs::rename(&tmp, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}
