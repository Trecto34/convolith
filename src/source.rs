//! Source abstraction and conservative format detection.

use crate::timeutil::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single readable input: a loose file, a directory entry belonging to a
/// logical source, or a file extracted from an archive into staging.
#[derive(Debug, Clone)]
pub struct Source {
    pub source_id: String,
    /// Human-readable path, including archive ancestry:
    /// `server-backup.tar.gz!/home/u/.codex/sessions/x.jsonl`.
    pub display_path: String,
    /// Outermost container first.
    pub container_chain: Vec<String>,
    /// Where the bytes can be read right now (real path or staging copy).
    pub read_path: PathBuf,
    /// Path as it appeared inside the container, if any.
    pub inner_path: Option<String>,
    pub size: u64,
    pub mtime: Option<Utc>,
    pub sha256: Option<String>,
    pub machine_id: Option<String>,
    pub platform: Option<String>,
    /// Labels carried from detection; part of the inventory, never identity.
    pub provider_label: Option<String>,
    pub application_label: Option<String>,
    pub parser_id: Option<String>,
}

impl Source {
    pub fn is_dir(&self) -> bool {
        self.read_path.is_dir()
    }
    /// Provider label captured at detection time, when one was assigned.
    pub fn provider_guess(&self) -> Option<&str> {
        self.provider_label.as_deref()
    }
    pub fn application_guess(&self) -> Option<&str> {
        self.application_label.as_deref()
    }
    pub fn parser_id(&self) -> Option<&str> {
        self.parser_id.as_deref()
    }
    /// The real path on disk, as opposed to the display path with archive
    /// ancestry. Used for the inventory's `original_path` when the file was
    /// extracted from a container, where the display path is not openable.
    pub fn full_path_for_record(&self) -> String {
        match &self.inner_path {
            Some(inner) if self.is_staged() => {
                // Keep the outer archive plus the member path: this is what an
                // operator needs to find the record again by hand.
                let outer: Vec<String> = self
                    .container_chain
                    .iter()
                    .take(self.container_chain.len().saturating_sub(1))
                    .cloned()
                    .collect();
                if outer.is_empty() {
                    inner.clone()
                } else {
                    format!("{}!/{}", outer.join("!/"), inner)
                }
            }
            // Equals the read path, except for files staged from another
            // machine, whose display path is the original location.
            _ => self.display_path.clone(),
        }
    }
    /// True when the bytes live in a temporary staging copy that the importer
    /// will delete after parsing.
    pub fn is_staged(&self) -> bool {
        !self.container_chain.is_empty()
    }
}

/// Cheap header used by `detect`: never reads the whole file.
#[derive(Debug, Clone)]
pub struct Probe {
    pub path: PathBuf,
    /// Parent directory name, for store-shape signals.
    pub parent_name: Option<String>,
    /// Names of sibling files (bounded).
    pub siblings: Vec<String>,
    pub size: u64,
    /// First N bytes, lossily decoded.
    pub head: String,
    /// First N bytes, raw.
    pub head_bytes: Vec<u8>,
    pub is_dir: bool,
    /// Path relative to the discovery root, POSIX-separated when possible.
    pub rel_path: String,
    pub full_path: String,
}

impl Probe {
    pub fn filename(&self) -> &str {
        self.path.file_name().and_then(|s| s.to_str()).unwrap_or("")
    }
    pub fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }
    pub fn head_lower(&self) -> String {
        self.head.to_ascii_lowercase()
    }
    pub fn sibling_exists(&self, name: &str) -> bool {
        self.siblings.iter().any(|s| s == name)
    }
    pub fn rel_contains(&self, needle: &str) -> bool {
        self.rel_path.contains(needle)
    }
    /// A path component match, e.g. `.codex` as a directory name.
    pub fn has_component(&self, name: &str) -> bool {
        std::path::Path::new(&self.full_path)
            .components()
            .any(|c| c.as_os_str() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Not this format.
    None,
    /// Plausible but not asserted as support.
    Weak,
    /// Structural evidence from the payload itself.
    Strong,
    /// Provider-mandated marker (own directory + own schema).
    Certain,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::None => "none",
            Confidence::Weak => "weak",
            Confidence::Strong => "strong",
            Confidence::Certain => "certain",
        }
    }
    pub fn score(self) -> u8 {
        match self {
            Confidence::None => 0,
            Confidence::Weak => 1,
            Confidence::Strong => 2,
            Confidence::Certain => 3,
        }
    }
}

/// `Detected` is not `supported`: detection is a claim about format identity,
/// support is a claim about successful extraction. They are reported separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    pub parser: String,
    pub provider: String,
    pub application: String,
    pub format: String,
    pub confidence: Confidence,
    pub reasons: Vec<String>,
}

impl Detection {
    pub fn none(parser: &str) -> Detection {
        Detection {
            parser: parser.to_string(),
            provider: String::new(),
            application: String::new(),
            format: String::new(),
            confidence: Confidence::None,
            reasons: vec!["no signal".into()],
        }
    }
    pub fn hit(
        parser: &str,
        provider: &str,
        application: &str,
        format: &str,
        confidence: Confidence,
        reason: impl Into<String>,
    ) -> Detection {
        Detection {
            parser: parser.to_string(),
            provider: provider.to_string(),
            application: application.to_string(),
            format: format.to_string(),
            confidence,
            reasons: vec![reason.into()],
        }
    }
    pub fn with_reason(mut self, reason: impl Into<String>) -> Detection {
        self.reasons.push(reason.into());
        self
    }
    pub fn is_hit(&self) -> bool {
        self.confidence > Confidence::None
    }
}

/// What a parser can actually recover. Truthful statuses only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub detect: bool,
    pub parse: bool,
    pub tool_calls: bool,
    pub attachments: bool,
    pub reasoning: bool,
    pub streaming: bool,
    /// Parser knowingly ignores parts of the source and records omissions.
    pub partial: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseReport {
    pub conversations: u64,
    pub events: u64,
    pub tool_calls: u64,
    pub records_examined: u64,
    pub records_failed: u64,
    pub records_skipped: u64,
    pub notes: Vec<String>,
}

impl ParseReport {
    pub fn merge(&mut self, other: &ParseReport) {
        self.conversations += other.conversations;
        self.events += other.events;
        self.tool_calls += other.tool_calls;
        self.records_examined += other.records_examined;
        self.records_failed += other.records_failed;
        self.records_skipped += other.records_skipped;
        self.notes.extend(other.notes.iter().cloned());
    }
}
