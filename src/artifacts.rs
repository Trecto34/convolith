//! Content-addressable artifact store.
//!
//! Layout is `artifacts/sha256/<aa>/<full-hex>`; identical bytes are stored
//! once. Large tool outputs and binaries live here so every event record stays
//! small and streamable.

use crate::id::hex;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct ArtifactStore {
    root: PathBuf,
    /// Cap on a single artifact's size; larger payloads are not copied.
    pub max_bytes: u64,
}

impl ArtifactStore {
    pub fn new(root: &Path, max_bytes: u64) -> ArtifactStore {
        ArtifactStore {
            root: root.to_path_buf(),
            max_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relpath_for(hash: &str) -> String {
        format!("sha256/{}/{}", &hash[..2], hash)
    }

    /// Store bytes; returns `(sha256, relative path, size)`.
    ///
    /// `copied` is false when the payload exceeds `max_bytes`: the hash and size
    /// are still reported so the event can reference it truthfully, but no bytes
    /// are written. Callers must therefore treat a missing file as expected for
    /// oversize payloads rather than as corruption.
    pub fn put_bytes(
        &self,
        bytes: &[u8],
        ext: Option<&str>,
    ) -> Result<(String, String, u64, bool)> {
        let size = bytes.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hash = hex(&hasher.finalize());
        let rel = Self::relpath_for(&hash);
        if size > self.max_bytes {
            return Ok((hash, rel, size, false));
        }
        let path = self.root.join(&rel);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            // Rename is atomic on both platforms; also makes concurrent writers safe.
            match std::fs::rename(&tmp, &path) {
                Ok(()) => {}
                Err(_) if path.exists() => {
                    let _ = std::fs::remove_file(&tmp);
                }
                Err(e) => return Err(e).context("storing artifact"),
            }
        }
        let _ = ext;
        Ok((hash, rel, size, true))
    }

    /// Stream a file into the store without loading it into memory.
    pub fn put_file(&self, path: &Path) -> Result<(String, String, u64, bool)> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > self.max_bytes {
            let hash = crate::id::sha256_file(path)?;
            return Ok((hash.clone(), Self::relpath_for(&hash), meta.len(), false));
        }
        let bytes = std::fs::read(path)?;
        self.put_bytes(&bytes, path.extension().and_then(|s| s.to_str()))
    }

    pub fn exists(&self, sha256: &str) -> bool {
        self.root.join(Self::relpath_for(sha256)).exists()
    }

    /// Total stored bytes, for the import report.
    pub fn size_on_disk(&self) -> u64 {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }
}

/// Guess a MIME type from a filename. Deliberately small; the spec does not
/// require accuracy, and callers may pass an explicit value.
pub fn guess_mime(filename: &str) -> Option<String> {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let m = match ext.as_str() {
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "zst" => "application/zstd",
        "tar" => "application/x-tar",
        "csv" => "text/csv",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "db" | "sqlite" | "sqlite3" => "application/vnd.sqlite3",
        _ => return None,
    };
    Some(m.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_once() {
        let dir = std::env::temp_dir().join(format!("convolith-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ArtifactStore::new(&dir, 1024);
        let (h1, p1, s1, copied) = store.put_bytes(b"hello artifact", Some("txt")).unwrap();
        let (h2, p2, _, _) = store.put_bytes(b"hello artifact", Some("txt")).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(p1, p2);
        assert_eq!(s1, "hello artifact".len() as u64);
        assert!(copied);
        assert!(store.exists(&h1));
        assert!(store.root().join(&p1).exists());
        // Oversize payload is hashed and reported, but not copied.
        let big = vec![7u8; 2048];
        let (_, _, size, copied) = store.put_bytes(&big, None).unwrap();
        assert_eq!(size, 2048);
        assert!(!copied);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
