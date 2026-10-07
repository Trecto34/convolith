//! Optional user configuration: machine aliases, project/repository aliases and
//! hard limits.
//!
//! A path is never treated as a machine identity on its own: a machine id only
//! appears when the operator declares it (here or with `--machine`).

use crate::archive::ArchiveLimits;
use crate::secrets::SecretPolicy;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub machine: Vec<MachineRule>,
    #[serde(default)]
    pub project_alias: Vec<ProjectAlias>,
    #[serde(default)]
    pub repository_alias: Vec<RepositoryAlias>,
    #[serde(default)]
    pub secrets: SecretsSection,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineRule {
    pub id: String,
    /// Path prefixes (as discovered) that identify this machine.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub platform: Option<String>,
    /// Alternate spellings for the same machine (e.g. WSL view of a Windows box).
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAlias {
    pub project: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub repository: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryAlias {
    pub repository: String,
    /// Remote URL spellings that mean the same repository.
    #[serde(default)]
    pub remotes: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsSection {
    #[serde(default = "default_policy")]
    pub policy: String,
}

fn default_policy() -> String {
    "redact".into()
}

impl Default for SecretsSection {
    fn default() -> Self {
        SecretsSection {
            policy: default_policy(),
        }
    }
}

/// Hard limits. Defaults are generous for a personal corpus but finite, so a
/// hostile input cannot exhaust the machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Skip files larger than this during discovery (bytes).
    #[serde(default = "d_max_file")]
    pub max_file_bytes: u64,
    /// Longest JSON/JSONL record we will parse (bytes).
    #[serde(default = "d_max_record")]
    pub max_record_bytes: usize,
    /// Largest artifact copied into the store (bytes).
    #[serde(default = "d_max_artifact")]
    pub max_artifact_bytes: u64,
    /// Largest total staged extraction for one archive (bytes).
    #[serde(default = "d_max_stage")]
    pub max_stage_bytes: u64,
    /// Files examined per import run.
    #[serde(default = "d_max_files")]
    pub max_files: u64,
    #[serde(default = "d_max_depth")]
    pub max_depth: usize,
    /// Longest string kept verbatim inside an event; longer text is stored as an
    /// artifact and referenced (never truncated away).
    #[serde(default = "d_max_inline")]
    pub max_inline_text_bytes: usize,
    /// Keep a case-insensitive set of extensions out of discovery.
    #[serde(default = "d_skip_ext")]
    pub skip_extensions: Vec<String>,
}

fn d_max_file() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn d_max_record() -> usize {
    64 * 1024 * 1024
}
fn d_max_artifact() -> u64 {
    512 * 1024 * 1024
}
fn d_max_stage() -> u64 {
    64 * 1024 * 1024 * 1024
}
fn d_max_files() -> u64 {
    2_000_000
}
fn d_max_depth() -> usize {
    48
}
fn d_max_inline() -> usize {
    32 * 1024
}
fn d_skip_ext() -> Vec<String> {
    [
        "png", "jpg", "jpeg", "gif", "webp", "heic", "mp4", "mov", "avi", "mkv", "mp3", "wav",
        "zip", "tar", "gz", "tgz", "zst", "zstd", "7z", "rar", "exe", "dll", "so", "dylib", "bin",
        "pak", "ttf", "otf", "woff", "woff2", "ico",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: d_max_file(),
            max_record_bytes: d_max_record(),
            max_artifact_bytes: d_max_artifact(),
            max_stage_bytes: d_max_stage(),
            max_files: d_max_files(),
            max_depth: d_max_depth(),
            max_inline_text_bytes: d_max_inline(),
            skip_extensions: d_skip_ext(),
        }
    }
}

impl Limits {
    pub fn archive(&self) -> ArchiveLimits {
        ArchiveLimits {
            max_total_uncompressed: self.max_stage_bytes.min(128 * 1024 * 1024 * 1024),
            ..ArchiveLimits::default()
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read config {path:?}"))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse config {path:?}"))?;
        Ok(cfg)
    }

    pub fn secret_policy(&self, override_policy: Option<SecretPolicy>) -> Result<SecretPolicy> {
        if let Some(p) = override_policy {
            return Ok(p);
        }
        SecretPolicy::parse(&self.secrets.policy)
            .ok_or_else(|| anyhow::anyhow!("unknown secret policy {:?}", self.secrets.policy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_machine_and_alias_config() {
        let text = r#"
[[machine]]
id = "desktop-windows"
paths = ["C:\\", "/mnt/c/"]
platform = "windows"

[[machine]]
id = "gpu-server"
paths = ["/srv/backups/host-a"]

[[project_alias]]
project = "foo"
paths = ["C:\\src\\foo", "/mnt/c/src/foo"]

[[repository_alias]]
repository = "github.com/me/foo"
remotes = ["git@github.com:me/foo.git", "https://github.com/me/foo"]

[secrets]
policy = "preserve"

[limits]
max_files = 10
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.machine.len(), 2);
        assert_eq!(cfg.machine[0].id, "desktop-windows");
        assert_eq!(cfg.project_alias[0].project, "foo");
        assert_eq!(cfg.secrets.policy, "preserve");
        assert_eq!(cfg.limits.max_files, 10);
        assert_eq!(cfg.limits.max_record_bytes, d_max_record());
    }

    #[test]
    fn rejects_unknown_keys() {
        let bad = "[nope]\nx = 1\n";
        assert!(toml::from_str::<Config>(bad).is_err());
    }
}
