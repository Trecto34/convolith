//! Canonical dataset layout, manifest, streaming writers and checksums.
//!
//! The dataset is designed so that it stays readable without this tool:
//!
//! ```text
//! canonical-ai-history/
//!   manifest.json                 dataset-level metadata and shard index
//!   README.md                     human overview
//!   schema/*.schema.json          the public specification, copied in
//!   data/events/part-000001.jsonl.zst   append-only canonical events
//!   data/aggregates/*.jsonl.zst         regenerated summaries
//!   artifacts/sha256/aa/<hex>     content-addressed blobs
//!   indexes/search.sqlite         derived FTS index (rebuildable)
//!   provenance/provenance.sqlite  the ledger: sources, observations, conflicts
//!   reports/*.md|json             human and machine reports
//!   checksums.sha256
//! ```
//!
//! `data/events/*.jsonl.zst` is authoritative. Everything else is either a
//! derived summary or provenance metadata; deleting the indexes never loses
//! history, and every file is listed with a SHA-256 in the manifest and in
//! `checksums.sha256`.

use crate::id::{hex, sha256_file};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const DATASET_FORMAT: &str = "convolith-canonical";
/// Pre-rename format id, still accepted when opening or validating existing archives.
pub const LEGACY_DATASET_FORMAT: &str = "aichive-canonical";
pub const DATASET_FORMAT_VERSION: u32 = 1;

pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn manifest(&self) -> PathBuf {
        self.root.join("manifest.json")
    }
    pub fn readme(&self) -> PathBuf {
        self.root.join("README.md")
    }
    pub fn schema_dir(&self) -> PathBuf {
        self.root.join("schema")
    }
    pub fn events_dir(&self) -> PathBuf {
        self.root.join("data").join("events")
    }
    pub fn aggregates_dir(&self) -> PathBuf {
        self.root.join("data").join("aggregates")
    }
    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }
    pub fn indexes(&self) -> PathBuf {
        self.root.join("indexes")
    }
    pub fn provenance_db(&self) -> PathBuf {
        self.root.join("provenance").join("provenance.sqlite")
    }
    pub fn reports(&self) -> PathBuf {
        self.root.join("reports")
    }
    pub fn checksums(&self) -> PathBuf {
        self.root.join("checksums.sha256")
    }
    pub fn scratch_parent(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// Create every directory the dataset needs. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.root.clone(),
            self.root.join("data"),
            self.events_dir(),
            self.aggregates_dir(),
            self.artifacts(),
            self.indexes(),
            self.root.join("provenance"),
            self.reports(),
            self.schema_dir(),
            self.scratch_parent(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("create {d:?}"))?;
        }
        Ok(())
    }

    /// True when a dataset (rather than an arbitrary directory) lives here.
    pub fn is_dataset(&self) -> bool {
        self.manifest().is_file() || self.provenance_db().is_file()
    }

    /// Next free event shard number.
    pub fn next_shard_index(&self) -> Result<u32> {
        let mut max = 0u32;
        let dir = self.events_dir();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(rest) = name.strip_prefix("part-") {
                    if let Some(num) = rest.split('.').next() {
                        if let Ok(n) = num.parse::<u32>() {
                            max = max.max(n);
                        }
                    }
                }
            }
        }
        Ok(max + 1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardRef {
    pub path: String,
    pub records: u64,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub import_run: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestCounts {
    pub sources: u64,
    pub conversations: u64,
    pub sessions: u64,
    pub events: u64,
    pub tool_calls: u64,
    pub artifacts: u64,
    pub projects: u64,
    pub machines: u64,
    pub conflicts: u64,
    pub redacted_events: u64,
    pub date_range: Option<[String; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRunRecord {
    pub import_run: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub tool_version: String,
    pub schema_version: u32,
    pub secret_policy: String,
    pub args: Vec<String>,
    pub events_new: u64,
    pub events_duplicate: u64,
    pub sources_examined: u64,
    pub sources_skipped: u64,
    pub sources_failed: u64,
    pub parse_errors: u64,
    pub state: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub format_version: u32,
    pub schema_version: u32,
    pub tool: String,
    pub tool_version: String,
    pub created_at: String,
    pub updated_at: String,
    pub redaction_policy: String,
    pub counts: ManifestCounts,
    pub event_shards: Vec<ShardRef>,
    #[serde(default)]
    pub derived_files: Vec<ShardRef>,
    #[serde(default)]
    pub import_runs: Vec<ImportRunRecord>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Manifest {
    pub fn new(tool_version: &str, policy: &str, now: &str) -> Manifest {
        Manifest {
            format: DATASET_FORMAT.into(),
            format_version: DATASET_FORMAT_VERSION,
            schema_version: crate::model::SCHEMA_VERSION,
            tool: "convolith".into(),
            tool_version: tool_version.into(),
            created_at: now.into(),
            updated_at: now.into(),
            redaction_policy: policy.into(),
            counts: ManifestCounts::default(),
            event_shards: Vec::new(),
            derived_files: Vec::new(),
            import_runs: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn load(root: &Path) -> Option<Manifest> {
        let text = std::fs::read_to_string(root.join("manifest.json")).ok()?;
        match serde_json::from_str::<Manifest>(&text) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!(
                    "convolith: manifest.json is not readable ({e}); continuing with defaults"
                );
                None
            }
        }
    }
}

/// Write bytes to `path` atomically (temp file in the same directory + rename),
/// so an interrupted run can never leave a half-written canonical file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

/// Streaming writer for `<stream>.jsonl.zst`.
///
/// Records are written one at a time; nothing accumulates in memory. The
/// compressed output is hashed after closing so the manifest can pin it.
pub struct JsonlZstWriter {
    // `Option` so `finish` can move the encoder out despite the `Drop` impl
    // that cleans up abandoned temp files.
    enc: Option<zstd::stream::write::Encoder<'static, std::io::BufWriter<std::fs::File>>>,
    tmp: PathBuf,
    final_path: PathBuf,
    records: u64,
    closed: bool,
}

impl JsonlZstWriter {
    pub fn create(final_path: &Path, level: i32) -> Result<JsonlZstWriter> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path(final_path);
        let f = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        let enc = zstd::stream::write::Encoder::new(std::io::BufWriter::new(f), level)
            .with_context(|| format!("zstd encoder for {tmp:?}"))?;
        Ok(JsonlZstWriter {
            enc: Some(enc),
            tmp,
            final_path: final_path.to_path_buf(),
            records: 0,
            closed: false,
        })
    }

    pub fn write_record<T: Serialize>(&mut self, value: &T) -> Result<()> {
        // A single record must never be large enough to matter: large payloads
        // are already moved into the artifact store by the caller.
        let s = serde_json::to_string(value)?;
        let enc = self.enc.as_mut().expect("writer used after finish");
        enc.write_all(s.as_bytes())?;
        enc.write_all(b"\n")?;
        self.records += 1;
        Ok(())
    }

    pub fn records(&self) -> u64 {
        self.records
    }

    /// Finish the stream and publish it. Returns the shard reference.
    pub fn finish(mut self, rel_path: &str, import_run: &str) -> Result<ShardRef> {
        self.closed = true;
        let enc = self.enc.take().expect("finished twice");
        let inner = enc.finish().context("finishing zstd stream")?;
        let f = inner
            .into_inner()
            .map_err(|e| anyhow::anyhow!("flush jsonl stream: {e}"))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.tmp, &self.final_path)?;
        let bytes = std::fs::metadata(&self.final_path)?.len();
        let sha256 = sha256_file(&self.final_path)?;
        Ok(ShardRef {
            path: rel_path.to_string(),
            records: self.records,
            bytes,
            sha256,
            import_run: import_run.to_string(),
        })
    }
}

impl Drop for JsonlZstWriter {
    fn drop(&mut self) {
        if !self.closed {
            // Abandoned mid-run: drop the partial temp file. The final path is
            // untouched, so a failed run cannot corrupt an existing dataset.
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Stream every JSON record out of a `.jsonl.zst` file. Records that fail to
/// parse are counted and skipped, never silently dropped.
pub fn read_jsonl_zst<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_record: usize,
    mut on_record: impl FnMut(T) -> Result<()>,
    on_error: &mut dyn FnMut(u64, String),
) -> Result<(u64, u64)> {
    let f = std::fs::File::open(path).with_context(|| format!("open {path:?}"))?;
    let dec = zstd::stream::read::Decoder::new(BufReader::new(f))
        .with_context(|| format!("zstd decode {path:?}"))?;
    let mut reader = BufReader::new(dec);
    let mut line = Vec::new();
    let mut ok = 0u64;
    let mut bad = 0u64;
    let mut idx = 0u64;
    loop {
        line.clear();
        let n = read_line_bounded(&mut reader, &mut line, max_record)?;
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        idx += 1;
        match serde_json::from_str::<T>(text) {
            Ok(v) => {
                on_record(v)?;
                ok += 1;
            }
            Err(e) => {
                bad += 1;
                on_error(idx, e.to_string());
            }
        }
    }
    Ok((ok, bad))
}

fn read_line_bounded(reader: &mut impl BufRead, out: &mut Vec<u8>, max: usize) -> Result<u64> {
    let mut total = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(pos) => {
                out.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                total += pos as u64 + 1;
                return Ok(total);
            }
            None => {
                let len = available.len();
                if out.len() + len > max {
                    bail!("record exceeds {max} bytes; refusing to buffer it");
                }
                out.extend_from_slice(available);
                reader.consume(len);
                total += len as u64;
            }
        }
    }
}

/// Write `checksums.sha256` covering every file the tool produced.
pub fn write_checksums(root: &Path, skip: &[PathBuf]) -> Result<usize> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for e in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        if skip.iter().any(|s| p == s) {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        // Reruns must not change a file just by re-listing it.
        if rel == "checksums.sha256" {
            continue;
        }
        match sha256_file(p) {
            Ok(h) => entries.push((rel, h)),
            Err(err) => eprintln!("convolith: warning: cannot hash {p:?}: {err}"),
        }
    }
    entries.sort();
    let mut out = String::new();
    for (rel, h) in &entries {
        out.push_str(&format!("{h}  {rel}\n"));
    }
    write_atomic(&root.join("checksums.sha256"), out.as_bytes())?;
    Ok(entries.len())
}

pub fn blake3_hex(bytes: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(bytes);
    hex(h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "convolith-ds-{label}-{}-{}",
            std::process::id(),
            crate::timeutil::now_utc().0
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_and_reads_back_jsonl_zst() {
        let d = tmpdir("jsonl");
        let layout = Layout { root: d.clone() };
        layout.ensure().unwrap();
        let target = layout.events_dir().join("part-000001.jsonl.zst");
        let mut w = JsonlZstWriter::create(&target, 3).unwrap();
        for i in 0..500u32 {
            w.write_record(&serde_json::json!({"i": i, "text": "hello"}))
                .unwrap();
        }
        let shard = w
            .finish("data/events/part-000001.jsonl.zst", "run1")
            .unwrap();
        assert_eq!(shard.records, 500);
        assert!(shard.bytes > 0);
        let mut seen = 0u64;
        let mut errs = Vec::new();
        let (ok, bad) = read_jsonl_zst::<serde_json::Value>(
            &target,
            1024 * 1024,
            |_v| {
                seen += 1;
                Ok(())
            },
            &mut |i, e| errs.push((i, e)),
        )
        .unwrap();
        assert_eq!((ok, bad, seen), (500, 0, 500));
        assert!(errs.is_empty());
        assert_eq!(layout.next_shard_index().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupted_record_is_counted_not_hidden() {
        let d = tmpdir("corrupt");
        let p = d.join("x.jsonl.zst");
        let f = std::fs::File::create(&p).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(f, 1).unwrap();
        enc.write_all(b"{\"a\":1}\nnot json\n{\"a\":2}\n").unwrap();
        enc.finish().unwrap();
        let mut bad = Vec::new();
        let (ok, nbad) =
            read_jsonl_zst::<serde_json::Value>(&p, 1024, |_| Ok(()), &mut |i, e| bad.push((i, e)))
                .unwrap();
        assert_eq!((ok, nbad), (2, 1));
        assert_eq!(bad[0].0, 2, "the failing line number is reported");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn abandoned_writer_leaves_no_file() {
        let d = tmpdir("abandon");
        let target = d.join("part-000001.jsonl.zst");
        {
            let mut w = JsonlZstWriter::create(&target, 1).unwrap();
            w.write_record(&serde_json::json!({"x": 1})).unwrap();
        }
        assert!(!target.exists(), "partial output must not be published");
        let leftovers: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert!(
            leftovers.is_empty(),
            "temp file must be removed: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn checksums_are_stable_across_runs() {
        let d = tmpdir("sums");
        std::fs::write(d.join("a.txt"), b"one").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/b.txt"), b"two").unwrap();
        let n1 = write_checksums(&d, &[]).unwrap();
        let first = std::fs::read_to_string(d.join("checksums.sha256")).unwrap();
        let n2 = write_checksums(&d, &[]).unwrap();
        let second = std::fs::read_to_string(d.join("checksums.sha256")).unwrap();
        assert_eq!(n1, n2);
        assert_eq!(first, second);
        assert!(first.contains("sub/b.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
