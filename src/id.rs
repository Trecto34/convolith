//! Deterministic identifiers.
//!
//! Every canonical id is a pure function of the coordinates that identify the
//! record, so re-importing the same bytes yields the same ids. BLAKE3 is used
//! for identity (fast, no length-extension surprise); SHA-256 is used where the
//! public spec and tooling expect it (artifact paths, `checksums.sha256`).

use blake3::Hasher;

/// Hash a tuple of strings, each length-prefixed so that `("ab","c")` and
/// `("a","bc")` cannot collide.
pub fn hash_parts(parts: &[&str]) -> [u8; 32] {
    let mut h = Hasher::new();
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
        h.update(b"\x1f");
    }
    *h.finalize().as_bytes()
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// `prefix` + 24 hex chars (96 bits) — collision-safe for personal archives and
/// short enough to paste into a terminal.
pub fn id(prefix: &str, parts: &[&str]) -> String {
    let h = hex(&hash_parts(parts));
    format!("{prefix}{}", &h[..24])
}

pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

pub fn blake3_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Hasher::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex(h.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(id("ev_", &["a", "b"]), id("ev_", &["a", "b"]));
        assert_ne!(id("ev_", &["ab", "c"]), id("ev_", &["a", "bc"]));
        assert!(id("ev_", &["x"]).starts_with("ev_"));
        assert_eq!(id("ev_", &["x"]).len(), 3 + 24);
    }
}
