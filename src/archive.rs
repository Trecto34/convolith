//! Safe archive inspection and bounded extraction.
//!
//! Archives are treated as hostile input:
//!
//! * entry names are sanitized (no absolute paths, no `..`, no drive letters,
//!   no NUL, POSIX-normalized) and rejected when they cannot be sanitized;
//! * symlinks, hardlinks, devices and FIFOs are never materialized;
//! * per-entry, per-archive and nesting limits bound expansion (zip bombs);
//! * nothing is written outside the importer-owned staging directory.
//!
//! Container ancestry is preserved: `server-backup.tar.gz!/home/u/.codex/x.jsonl`.

use crate::timeutil::Utc;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
    Gzip,
    Zstd,
}

impl ArchiveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveKind::Zip => "zip",
            ArchiveKind::Tar => "tar",
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::Gzip => "gzip",
            ArchiveKind::Zstd => "zstd",
        }
    }
}

/// Classify by magic bytes first, extension second. Magic wins so that a
/// renamed archive is still handled safely instead of being read as text.
pub fn classify(head: &[u8], filename: &str) -> Option<ArchiveKind> {
    let name = filename.to_ascii_lowercase();
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(ArchiveKind::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            return Some(ArchiveKind::TarGz);
        }
        return Some(ArchiveKind::Gzip);
    }
    if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        // Zstandard frame magic (little endian 0xFD2FB528)
        return Some(ArchiveKind::Zstd);
    }
    if head.len() > 262 && &head[257..262] == b"ustar" {
        return Some(ArchiveKind::Tar);
    }
    // Fall back to the extension only when the magic is inconclusive.
    if name.ends_with(".zip") || name.ends_with(".jar") {
        return Some(ArchiveKind::Zip);
    }
    if name.ends_with(".tar") {
        return Some(ArchiveKind::Tar);
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Some(ArchiveKind::TarGz);
    }
    if name.ends_with(".gz") {
        return Some(ArchiveKind::Gzip);
    }
    if name.ends_with(".zst") || name.ends_with(".zstd") || name.ends_with(".zstd") {
        return Some(ArchiveKind::Zstd);
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub max_entries: u64,
    pub max_total_uncompressed: u64,
    pub max_entry_bytes: u64,
    pub max_depth: u32,
    /// Abort when a single entry expands more than this many times its
    /// compressed size (and is larger than 4 MiB).
    pub max_ratio: u64,
    pub max_path_len: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        ArchiveLimits {
            max_entries: 1_000_000,
            max_total_uncompressed: 128 * 1024 * 1024 * 1024,
            max_entry_bytes: 8 * 1024 * 1024 * 1024,
            max_depth: 4,
            max_ratio: 2_000,
            max_path_len: 1024,
        }
    }
}

impl ExtractedEntry {
    /// Cheap clone for callers that collect entries while the walk continues
    /// (the staging dir outlives the callback but not the walk).
    pub fn clone_entry(&self) -> ExtractedEntry {
        ExtractedEntry {
            inner_path: self.inner_path.clone(),
            staged_path: self.staged_path.clone(),
            size: self.size,
            mtime: self.mtime,
            is_nested_archive: self.is_nested_archive,
        }
    }
}

#[derive(Debug)]
pub struct ExtractedEntry {
    pub inner_path: String,
    pub staged_path: PathBuf,
    pub size: u64,
    pub mtime: Option<Utc>,
    /// True when the entry is itself an archive and was recursed into.
    pub is_nested_archive: bool,
}

#[derive(Debug, Default)]
pub struct ArchiveSummary {
    pub entries_seen: u64,
    pub entries_extracted: u64,
    pub entries_skipped: u64,
    pub bytes_written: u64,
    pub nested_archives: u64,
    pub notes: Vec<String>,
    /// `(entry name, reason)` for entries whose bytes could not be read
    /// (corrupt data, bad CRC). The rest of the archive is still processed.
    pub entries_failed: Vec<(String, String)>,
}

/// Sanitize an archive entry name into a safe relative path.
///
/// Returns `None` for anything that cannot be represented safely; callers count
/// those as skipped and record the reason.
pub fn sanitize_entry_path(name: &str, max_len: usize) -> Option<PathBuf> {
    if name.is_empty() || name.len() > max_len || name.contains('\0') {
        return None;
    }
    // Reject Windows drive prefixes and UNC.
    let mut trimmed = name.replace('\\', "/");
    // `./C:` is the drive-relative `C:` once the no-op leading segments go.
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest.to_string();
    }
    if trimmed.starts_with("//") || trimmed.starts_with('/') {
        return None;
    }
    if trimmed.len() >= 2
        && trimmed.as_bytes()[1] == b':'
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
    {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in trimmed.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return None;
        }
        // Windows reserved device names, with or without extension.
        let stem = comp.split('.').next().unwrap_or("").to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            return None;
        }
        if comp.len() > 255 {
            return None;
        }
        out.push(comp);
    }
    // Belt and braces: the joined path must stay relative.
    if out.as_os_str().is_empty() || out.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(out)
}

fn staged_dest(staging: &Path, depth: u32, seq: u64, rel: &Path) -> PathBuf {
    let mut dest = staging.join(format!("d{depth}-{seq}"));
    dest.push(rel);
    dest
}

/// Walk an archive, staging each regular file entry under `staging`.
///
/// `on_entry` is invoked for every successfully staged file (including nested
/// archives, which are also staged so callers can hash or detect them). When
/// `recurse_nested` is set, nested archives are additionally walked.
#[allow(clippy::too_many_arguments)]
pub fn walk_archive<F>(
    path: &Path,
    kind: ArchiveKind,
    staging: &Path,
    depth: u32,
    limits: &ArchiveLimits,
    recurse_nested: bool,
    container_chain: &[String],
    on_entry: &mut F,
) -> Result<ArchiveSummary>
where
    F: FnMut(&ExtractedEntry, &[String]) -> Result<()>,
{
    if depth > limits.max_depth {
        bail!(
            "archive nesting depth {} exceeds limit {}",
            depth,
            limits.max_depth
        );
    }
    let mut summary = ArchiveSummary::default();
    match kind {
        ArchiveKind::Zip => walk_zip(
            path,
            staging,
            depth,
            limits,
            recurse_nested,
            container_chain,
            on_entry,
            &mut summary,
        )?,
        ArchiveKind::Tar | ArchiveKind::TarGz => walk_tar(
            path,
            kind,
            staging,
            depth,
            limits,
            recurse_nested,
            container_chain,
            on_entry,
            &mut summary,
        )?,
        ArchiveKind::Gzip => walk_single_stream(
            path,
            ArchiveKind::Gzip,
            staging,
            depth,
            limits,
            recurse_nested,
            container_chain,
            on_entry,
            &mut summary,
        )?,
        ArchiveKind::Zstd => walk_single_stream(
            path,
            ArchiveKind::Zstd,
            staging,
            depth,
            limits,
            recurse_nested,
            container_chain,
            on_entry,
            &mut summary,
        )?,
    }
    Ok(summary)
}

/// Strip a compression extension so `session.jsonl.zstd` stages as
/// `session.jsonl` and detection sees the real format.
fn decompressed_name(name: &str, kind: ArchiveKind) -> String {
    let lower = name.to_ascii_lowercase();
    let suffixes: &[&str] = match kind {
        ArchiveKind::Gzip => &[".gz", ".gzip"],
        ArchiveKind::Zstd => &[".zst", ".zstd"],
        _ => &[],
    };
    for s in suffixes {
        if lower.ends_with(s) {
            return name[..name.len() - s.len()].to_string();
        }
    }
    name.to_string()
}

#[allow(clippy::too_many_arguments)]
fn walk_single_stream<F>(
    path: &Path,
    kind: ArchiveKind,
    staging: &Path,
    depth: u32,
    limits: &ArchiveLimits,
    recurse_nested: bool,
    container_chain: &[String],
    on_entry: &mut F,
    summary: &mut ArchiveSummary,
) -> Result<()>
where
    F: FnMut(&ExtractedEntry, &[String]) -> Result<()>,
{
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("stream");
    let inner = decompressed_name(name, kind);
    let rel = sanitize_entry_path(&inner, limits.max_path_len)
        .unwrap_or_else(|| PathBuf::from("stream.bin"));
    let dest = staged_dest(staging, depth, 0, &rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    summary.entries_seen += 1;
    let mut out = std::fs::File::create(&dest)?;
    let written = match kind {
        ArchiveKind::Gzip => {
            let f = std::fs::File::open(path)?;
            let mut dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(f));
            copy_limited(
                &mut dec,
                &mut out,
                limits,
                summary,
                Some(std::fs::metadata(path)?.len()),
            )?
        }
        ArchiveKind::Zstd => {
            let f = std::fs::File::open(path)?;
            let mut dec = zstd::stream::read::Decoder::new(std::io::BufReader::new(f))?;
            copy_limited(
                &mut dec,
                &mut out,
                limits,
                summary,
                Some(std::fs::metadata(path)?.len()),
            )?
        }
        _ => unreachable!(),
    };
    out.flush()?;
    drop(out);
    summary.entries_extracted += 1;
    summary.bytes_written += written;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(crate::timeutil::from_system_time);
    let entry = ExtractedEntry {
        inner_path: inner.clone(),
        staged_path: dest.clone(),
        size: written,
        mtime,
        is_nested_archive: false,
    };
    let mut chain = container_chain.to_vec();
    chain.push(
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
    );
    on_entry(&entry, &chain)?;
    // A tar.gz decompresses to a tar; recurse one level so its members are seen.
    if let Ok(mut f) = std::fs::File::open(&dest) {
        let mut head = vec![0u8; 512];
        let n = f.read(&mut head).unwrap_or(0);
        head.truncate(n);
        if let Some(inner_kind) = classify(&head, &inner) {
            if inner_kind != kind && recurse_nested && depth < limits.max_depth {
                summary.nested_archives += 1;
                walk_archive(
                    &dest,
                    inner_kind,
                    staging,
                    depth + 1,
                    limits,
                    recurse_nested,
                    &chain,
                    on_entry,
                )?;
            }
        }
    }
    Ok(())
}

fn copy_limited<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    limits: &ArchiveLimits,
    summary: &mut ArchiveSummary,
    compressed_size: Option<u64>,
) -> Result<u64> {
    let mut buf = vec![0u8; 128 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > limits.max_entry_bytes {
            bail!(
                "entry expands beyond max_entry_bytes ({})",
                limits.max_entry_bytes
            );
        }
        if summary.bytes_written + written > limits.max_total_uncompressed {
            bail!("archive expands beyond max_total_uncompressed");
        }
        if let Some(compressed) = compressed_size {
            if written > 4 * 1024 * 1024 && written > compressed.saturating_mul(limits.max_ratio) {
                bail!("entry exceeds max_compression_ratio ({})", limits.max_ratio);
            }
        }
        writer.write_all(&buf[..n])?;
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)]
fn walk_tar<F>(
    path: &Path,
    kind: ArchiveKind,
    staging: &Path,
    depth: u32,
    limits: &ArchiveLimits,
    recurse_nested: bool,
    container_chain: &[String],
    on_entry: &mut F,
    summary: &mut ArchiveSummary,
) -> Result<()>
where
    F: FnMut(&ExtractedEntry, &[String]) -> Result<()>,
{
    let f = std::fs::File::open(path)?;
    let reader: Box<dyn Read> = match kind {
        ArchiveKind::TarGz => Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::new(
            f,
        ))),
        _ => Box::new(std::io::BufReader::new(f)),
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_ignore_zeros(true);
    let mut seq = 0u64;
    let mut chain = container_chain.to_vec();
    chain.push(
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
    );
    for entry in ar.entries()? {
        let mut entry = match entry {
            Ok(e) => e,
            Err(e) => {
                summary.entries_skipped += 1;
                summary.notes.push(format!("tar entry error: {e}"));
                // Empty name: the archive stream itself is damaged.
                summary
                    .entries_failed
                    .push((String::new(), format!("tar entry error: {e}")));
                continue;
            }
        };
        summary.entries_seen += 1;
        if summary.entries_seen > limits.max_entries {
            bail!("archive has more than {} entries", limits.max_entries);
        }
        let header = entry.header().clone();
        let etype = header.entry_type();
        if !etype.is_file() {
            // Symlinks, hardlinks, dirs, devices: never materialized.
            summary.entries_skipped += 1;
            continue;
        }
        let raw_name = match entry.path() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => {
                summary.entries_skipped += 1;
                continue;
            }
        };
        let Some(rel) = sanitize_entry_path(&raw_name, limits.max_path_len) else {
            summary.entries_skipped += 1;
            summary
                .notes
                .push(format!("rejected unsafe entry path: {raw_name}"));
            continue;
        };
        let declared = header.size().unwrap_or(0);
        if declared > limits.max_entry_bytes {
            summary.entries_skipped += 1;
            summary
                .notes
                .push(format!("entry too large: {raw_name} ({declared} bytes)"));
            continue;
        }
        seq += 1;
        let dest = staged_dest(staging, depth, seq, &rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        let written = match copy_limited(&mut entry, &mut out, limits, summary, None) {
            Ok(n) => n,
            // A read failure is damage to this entry only; limit violations stay fatal.
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                drop(out);
                let _ = std::fs::remove_file(&dest);
                summary.entries_skipped += 1;
                summary
                    .entries_failed
                    .push((raw_name.clone(), format!("{e:#}")));
                continue;
            }
            Err(e) => return Err(e),
        };
        out.flush()?;
        drop(out);
        summary.entries_extracted += 1;
        summary.bytes_written += written;
        // tar headers carry second-resolution mtimes; 0 means "unspecified".
        let mtime = header
            .mtime()
            .ok()
            .map(|s| Utc(s as i64 * 1_000_000_000))
            .filter(|u| u.0 > 0);
        let entry_rec = ExtractedEntry {
            inner_path: raw_name.clone(),
            staged_path: dest.clone(),
            size: written,
            mtime,
            is_nested_archive: false,
        };
        on_entry(&entry_rec, &chain)?;
        if recurse_nested && depth < limits.max_depth {
            if let Some(inner_kind) = sniff_archive(&dest) {
                if inner_kind != kind {
                    summary.nested_archives += 1;
                    walk_archive(
                        &dest,
                        inner_kind,
                        staging,
                        depth + 1,
                        limits,
                        recurse_nested,
                        &chain,
                        on_entry,
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_zip<F>(
    path: &Path,
    staging: &Path,
    depth: u32,
    limits: &ArchiveLimits,
    recurse_nested: bool,
    container_chain: &[String],
    on_entry: &mut F,
    summary: &mut ArchiveSummary,
) -> Result<()>
where
    F: FnMut(&ExtractedEntry, &[String]) -> Result<()>,
{
    let f = std::fs::File::open(path).with_context(|| format!("open zip {path:?}"))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f))
        .with_context(|| format!("read zip directory {path:?}"))?;
    let mut chain = container_chain.to_vec();
    chain.push(
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
    );
    let count = zip.len();
    for idx in 0..count {
        summary.entries_seen += 1;
        if summary.entries_seen > limits.max_entries {
            bail!("archive has more than {} entries", limits.max_entries);
        }
        let mut entry = match zip.by_index(idx) {
            Ok(e) => e,
            Err(e) => {
                summary.entries_skipped += 1;
                summary.notes.push(format!("zip entry {idx} error: {e}"));
                continue;
            }
        };
        if entry.is_dir() {
            summary.entries_skipped += 1;
            continue;
        }
        // Reject symlinks and other special unix modes.
        if let Some(mode) = entry.unix_mode() {
            let ftype = mode & 0o170000;
            if ftype == 0o120000 || (ftype != 0 && ftype != 0o100000) {
                summary.entries_skipped += 1;
                continue;
            }
        }
        let raw_name = entry.name().to_string();
        let Some(rel) = sanitize_entry_path(&raw_name, limits.max_path_len) else {
            summary.entries_skipped += 1;
            summary
                .notes
                .push(format!("rejected unsafe entry path: {raw_name}"));
            continue;
        };
        let declared = entry.size();
        if declared > limits.max_entry_bytes {
            summary.entries_skipped += 1;
            summary.notes.push(format!("entry too large: {raw_name}"));
            continue;
        }
        let compressed = entry.compressed_size().max(1);
        if declared > 4 * 1024 * 1024 && declared / compressed > limits.max_ratio {
            summary.entries_skipped += 1;
            summary
                .notes
                .push(format!("suspected compression bomb: {raw_name}"));
            continue;
        }
        let dest = staged_dest(staging, depth, idx as u64, &rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        let written = match copy_limited(&mut entry, &mut out, limits, summary, None) {
            Ok(n) => n,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                drop(out);
                let _ = std::fs::remove_file(&dest);
                summary.entries_skipped += 1;
                summary
                    .entries_failed
                    .push((raw_name.clone(), format!("{e:#}")));
                continue;
            }
            Err(e) => return Err(e),
        };
        out.flush()?;
        drop(out);
        summary.entries_extracted += 1;
        summary.bytes_written += written;
        // Zip stores MS-DOS local time without a zone. `last_modified` is the
        // only field available; treating it as UTC would invent an offset, so
        // the timestamp is kept as `unknown` and the raw DOS value is not
        // promoted to a canonical timestamp.
        let mtime: Option<Utc> = None;
        let entry_rec = ExtractedEntry {
            inner_path: raw_name.clone(),
            staged_path: dest.clone(),
            size: written,
            mtime,
            is_nested_archive: false,
        };
        on_entry(&entry_rec, &chain)?;
        if recurse_nested && depth < limits.max_depth {
            if let Some(inner_kind) = sniff_archive(&dest) {
                if inner_kind != ArchiveKind::Zip || depth == 0 {
                    summary.nested_archives += 1;
                    walk_archive(
                        &dest,
                        inner_kind,
                        staging,
                        depth + 1,
                        limits,
                        recurse_nested,
                        &chain,
                        on_entry,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Sniff a staged file for an archive magic.
pub fn sniff_archive(path: &Path) -> Option<ArchiveKind> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 512];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    classify(&head, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_unsafe_names() {
        assert!(sanitize_entry_path("../etc/passwd", 1024).is_none());
        assert!(sanitize_entry_path("/etc/passwd", 1024).is_none());
        assert!(sanitize_entry_path("C:\\Windows\\evil", 1024).is_none());
        assert!(sanitize_entry_path("\\\\server\\share", 1024).is_none());
        assert!(sanitize_entry_path("a\0b", 1024).is_none());
        assert!(sanitize_entry_path("CON", 1024).is_none());
        assert!(sanitize_entry_path("nul.txt", 1024).is_none());
        assert_eq!(
            sanitize_entry_path("./home/u/.codex/x.jsonl", 1024).unwrap(),
            PathBuf::from("home/u/.codex/x.jsonl")
        );
        assert_eq!(
            sanitize_entry_path("a\\b\\c", 1024).unwrap(),
            PathBuf::from("a/b/c")
        );
        assert!(sanitize_entry_path(&"x".repeat(2000), 1024).is_none());
    }

    #[test]
    fn classifies_by_magic() {
        assert_eq!(classify(b"PK\x03\x04rest", "x.zip"), Some(ArchiveKind::Zip));
        assert_eq!(
            classify(&[0x1f, 0x8b, 0x08, 0, 0], "x.tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            classify(&[0x1f, 0x8b, 0x08, 0, 0], "x.gz"),
            Some(ArchiveKind::Gzip)
        );
        assert_eq!(
            classify(&[0x28, 0xb5, 0x2f, 0xfd], "x.zstd"),
            Some(ArchiveKind::Zstd)
        );
        assert_eq!(classify(b"not an archive", "readme.txt"), None);
        assert_eq!(classify(b"", "empty.zip"), Some(ArchiveKind::Zip));
    }

    #[test]
    fn decompressed_names_strip_compression_suffix() {
        assert_eq!(
            decompressed_name("session.jsonl.zstd", ArchiveKind::Zstd),
            "session.jsonl"
        );
        assert_eq!(decompressed_name("a.tar.gz", ArchiveKind::Gzip), "a.tar");
        assert_eq!(decompressed_name("keep.txt", ArchiveKind::Gzip), "keep.txt");
    }
}
