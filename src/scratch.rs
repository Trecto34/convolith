//! Scratch directories with deterministic cleanup.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// A directory the importer owns and removes on drop.
pub struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    pub fn create(parent: &Path, label: &str) -> Result<Scratch> {
        let mut n = 0;
        loop {
            let path = parent.join(format!("{label}-{}-{n}", std::process::id(),));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Scratch { path, keep: false }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    n += 1;
                    if n > 10_000 {
                        anyhow::bail!("could not create scratch directory under {parent:?}");
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Leave the directory in place (used by `--keep-staging`).
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Recursively remove a directory, tolerating read-only files (Windows
/// backups frequently carry the read-only attribute).
pub fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) => {
            fn clear_ro(p: &Path) {
                if let Ok(md) = std::fs::metadata(p) {
                    let mut perm = md.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    perm.set_readonly(false);
                    let _ = std::fs::set_permissions(p, perm);
                }
            }
            fn walk(p: &Path) {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for e in rd.flatten() {
                        let child = e.path();
                        if child.is_dir() {
                            walk(&child);
                        } else {
                            clear_ro(&child);
                        }
                    }
                }
                clear_ro(p);
            }
            walk(path);
            std::fs::remove_dir_all(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_removed_on_drop() {
        let base = std::env::temp_dir().join(format!("convolith-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let p = {
            let s = Scratch::create(&base, "run").unwrap();
            std::fs::write(s.path().join("f.txt"), b"x").unwrap();
            s.path().to_path_buf()
        };
        assert!(!p.exists());
        let _ = remove_dir_all(&base);
    }
}
