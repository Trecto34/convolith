//! Shared by `tests/props.rs` and the cargo-fuzz targets: feed raw bytes to a
//! line-oriented parser through the public API and report the accounting.
//! The JSONL line reader is private to `convolith::parsers`, so it is exercised
//! the way production reaches it: through a parser's `parse`.

#![allow(dead_code)]

use convolith::model::EventDraft;
use convolith::parser::{ConversationMeta, EventSink, ParseContext, SourceParser};
use convolith::secrets::RedactionHit;
use convolith::source::{ParseReport, Source};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Collect {
    pub events: usize,
    open: bool,
}

impl EventSink for Collect {
    fn begin(&mut self, _: ConversationMeta) -> anyhow::Result<()> {
        assert!(!self.open, "begin before the previous end");
        self.open = true;
        Ok(())
    }
    fn emit(&mut self, _: EventDraft) -> anyhow::Result<()> {
        assert!(self.open, "emit outside begin/end");
        self.events += 1;
        Ok(())
    }
    fn end(&mut self) -> anyhow::Result<()> {
        assert!(self.open, "end without begin");
        self.open = false;
        Ok(())
    }
}

struct Ctx {
    staging: PathBuf,
    max_record: usize,
}

impl ParseContext for Ctx {
    fn staging_dir(&self) -> &Path {
        &self.staging
    }
    fn machine_id(&self) -> Option<&str> {
        None
    }
    fn platform(&self) -> Option<&str> {
        None
    }
    fn import_run(&self) -> &str {
        "fuzz"
    }
    fn provider(&self) -> &str {
        "fuzz"
    }
    fn application(&self) -> &str {
        "fuzz"
    }
    fn parser_id(&self) -> &str {
        "fuzz"
    }
    fn parser_version(&self) -> &str {
        "1"
    }
    fn max_inline_text_bytes(&self) -> usize {
        usize::MAX
    }
    fn store_artifact(
        &mut self,
        _: &[u8],
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("no artifacts in the harness")
    }
    fn apply_secret_policy(&self, _: &str, text: &str) -> (String, Option<RedactionHit>) {
        (text.to_string(), None)
    }
    fn max_record_bytes(&self) -> usize {
        self.max_record
    }
    fn note(&mut self, _: String) {}
}

/// Non-blank physical lines: a leading BOM is dropped, then a line is blank
/// when it is empty or only space, tab and carriage return (what `LineReader`
/// strips; other whitespace such as VT is not JSON whitespace and is a record
/// that fails to parse).
pub fn non_blank_lines(bytes: &[u8]) -> u64 {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.iter().all(|b| matches!(b, b'\r' | b' ' | b'\t')))
        .count() as u64
}

/// Parse `bytes` as a file; `Err` is a whole-source failure (allowed), a panic
/// or a broken sink contract is not.
pub fn parse_bytes(
    parser: &dyn SourceParser,
    bytes: &[u8],
    max_record: usize,
) -> anyhow::Result<(usize, ParseReport)> {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("convolith-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("in-{}.jsonl", N.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&path, bytes)?;
    let source = Source {
        source_id: "src_fuzz".into(),
        display_path: path.display().to_string(),
        container_chain: vec![],
        read_path: path.clone(),
        inner_path: None,
        size: bytes.len() as u64,
        mtime: None,
        sha256: None,
        machine_id: None,
        platform: None,
        provider_label: None,
        application_label: None,
        parser_id: None,
    };
    let mut sink = Collect::default();
    let mut ctx = Ctx {
        staging: dir.clone(),
        max_record,
    };
    let out = parser.parse(&mut ctx, &source, &mut sink);
    let _ = std::fs::remove_file(&path);
    assert!(!sink.open, "parser returned with a conversation still open");
    Ok((sink.events, out?))
}
