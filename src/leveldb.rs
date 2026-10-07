//! LevelDB (Chromium/Electron profile stores): detection and safe inspection.
//!
//! A LevelDB directory is never opened with a LevelDB engine — that would take
//! the `LOCK`, may write a new `LOG` and can trigger a compaction, all inside
//! the user's profile. Instead the component files are *copied* into tool-owned
//! staging and scanned as bytes. The scan only looks for provider evidence (a
//! known web origin in a key); it never decodes values into conversations. A
//! generic Electron store therefore stays `unresolved`: inventoried, counted,
//! and left for a decoder that can prove the provider.

use anyhow::Result;
use std::path::Path;

/// Provider evidence is an origin string found in the store: `(needle, provider)`.
const ORIGINS: &[(&str, &str)] = &[
    ("https://claude.ai", "anthropic"),
    ("https://chatgpt.com", "openai"),
    ("https://chat.openai.com", "openai"),
    ("https://gemini.google.com", "google"),
    ("https://copilot.microsoft.com", "microsoft"),
    ("https://www.perplexity.ai", "perplexity"),
];

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelDbReport {
    pub files: usize,
    pub table_files: usize,
    pub log_files: usize,
    pub bytes: u64,
    /// Providers whose web origin appears in the store.
    pub provider_evidence: Vec<String>,
    /// Why the store is not turned into conversations.
    pub note: String,
}

/// A directory is LevelDB only if it has both `CURRENT` and a `MANIFEST-*`:
/// stray `.log`/`.ldb` names prove nothing.
pub fn is_leveldb<S: AsRef<str>>(names: &[S]) -> bool {
    names.iter().any(|n| n.as_ref() == "CURRENT")
        && names.iter().any(|n| n.as_ref().starts_with("MANIFEST-"))
}

fn is_component(name: &str) -> bool {
    name == "CURRENT"
        || name == "LOG"
        || name == "LOG.old"
        || name.starts_with("MANIFEST-")
        || name.ends_with(".ldb")
        || name.ends_with(".log")
        || name.ends_with(".sst")
}

/// Copy the store's components into `staging` (read-only on the source, no
/// symlinks followed, size-capped) and scan the copies.
pub fn inspect(dir: &Path, staging: &Path) -> Result<LevelDbReport> {
    std::fs::create_dir_all(staging)?;
    let mut report = LevelDbReport {
        files: 0,
        table_files: 0,
        log_files: 0,
        bytes: 0,
        provider_evidence: Vec::new(),
        note: String::new(),
    };
    let mut truncated = false;
    let mut scanned: Vec<u8> = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(md) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if !md.is_file() || !is_component(&name) {
            continue;
        }
        if report.files >= MAX_FILES
            || report.bytes + md.len() > MAX_TOTAL_BYTES
            || md.len() > MAX_FILE_BYTES
        {
            truncated = true;
            continue;
        }
        let dst = staging.join(&name);
        if std::fs::copy(e.path(), &dst).is_err() {
            truncated = true;
            continue;
        }
        report.files += 1;
        report.bytes += md.len();
        if name.ends_with(".ldb") || name.ends_with(".sst") {
            report.table_files += 1;
        } else if name.ends_with(".log") {
            report.log_files += 1;
        }
        if let Ok(bytes) = std::fs::read(&dst) {
            for (needle, _) in ORIGINS {
                if contains(&bytes, needle.as_bytes()) {
                    scanned.extend_from_slice(needle.as_bytes());
                    scanned.push(0);
                }
            }
        }
    }
    for (needle, provider) in ORIGINS {
        if contains(&scanned, needle.as_bytes())
            && !report.provider_evidence.iter().any(|p| p == provider)
        {
            report.provider_evidence.push((*provider).to_string());
        }
    }
    report.provider_evidence.sort();
    report.note = if report.provider_evidence.is_empty() {
        "LevelDB store with no provider evidence; not interpreted as conversations (unresolved)"
            .into()
    } else {
        format!(
            "LevelDB store with {} origin evidence; no decoder proves its keys hold conversations (unresolved)",
            report.provider_evidence.join(",")
        )
    };
    if truncated {
        report
            .note
            .push_str("; some files exceeded inspection limits and were not copied");
    }
    Ok(report)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str, with_origin: Option<&str>) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("convolith-ldb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("CURRENT"), "MANIFEST-000001\n").unwrap();
        std::fs::write(d.join("MANIFEST-000001"), b"\x01\x02manifest").unwrap();
        std::fs::write(d.join("LOCK"), b"").unwrap();
        let mut body = b"\x00\x01_".to_vec();
        body.extend_from_slice(with_origin.unwrap_or("file://").as_bytes());
        body.extend_from_slice(b"\x00\x01theme\x00dark");
        std::fs::write(d.join("000003.log"), body).unwrap();
        d
    }

    #[test]
    fn detection_needs_current_and_manifest() {
        assert!(is_leveldb(&["CURRENT", "MANIFEST-000002", "x.ldb"]));
        assert!(!is_leveldb(&["000003.log", "x.ldb"]));
        assert!(!is_leveldb(&["CURRENT"]));
    }

    #[test]
    fn generic_store_is_unresolved_and_source_is_untouched() {
        let d = store("generic", None);
        let before = std::fs::read_dir(&d).unwrap().count();
        let stage =
            std::env::temp_dir().join(format!("convolith-ldb-stage-{}", std::process::id()));
        let r = inspect(&d, &stage).unwrap();
        assert!(r.provider_evidence.is_empty());
        assert!(r.note.contains("unresolved"));
        assert_eq!(r.log_files, 1);
        assert_eq!(
            std::fs::read_dir(&d).unwrap().count(),
            before,
            "source dir unchanged"
        );
        assert!(
            stage.join("000003.log").exists(),
            "inspection works on a copy"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&stage);
    }

    #[test]
    fn origin_is_evidence_but_still_unresolved() {
        let d = store("origin", Some("https://claude.ai"));
        let stage =
            std::env::temp_dir().join(format!("convolith-ldb-stage2-{}", std::process::id()));
        let r = inspect(&d, &stage).unwrap();
        assert_eq!(r.provider_evidence, vec!["anthropic"]);
        assert!(r.note.contains("unresolved"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&stage);
    }
}
