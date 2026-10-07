//! Full-text search over the canonical event shards (`indexes/search.sqlite`).
//!
//! The index is *derived*: it is built only from `data/**/*.jsonl.zst` and can
//! be deleted and rebuilt at any time without touching the history. The text it
//! stores has already passed the secret policy of the import that produced it,
//! and is scrubbed again here, so a search result can never surface a
//! credential that the canonical record does not carry.
//!
//! ```text
//! rebuild_index(ds)     # (re)creates indexes/search.sqlite
//! search(ds, "kw", 20)  # timestamp/provider/.../event_id/preview
//! ```

use crate::dataset::Layout;
use crate::model::Event;
use crate::secrets::scrub_for_log;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INDEX_FILE: &str = "search.sqlite";
/// Shards larger than this are refused rather than buffered whole.
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
/// Tokens of context returned around a match.
const SNIPPET_TOKENS: usize = 16;
/// Hard cap on a returned preview, so a hit can never dump a whole document.
const PREVIEW_CHARS: usize = 400;

const SCHEMA: &str = r#"
create virtual table if not exists event_fts using fts5(
    event_id unindexed,
    timestamp unindexed,
    provider unindexed,
    application unindexed,
    project unindexed,
    conversation unindexed,
    text,
    tokenize = 'unicode61 remove_diacritics 2'
);
"#;

/// Path of the derived search index for a dataset root.
pub fn index_path(dataset: &Path) -> PathBuf {
    dataset.join("indexes").join(INDEX_FILE)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub event_id: String,
    pub timestamp: Option<String>,
    pub provider: String,
    pub application: String,
    pub project: Option<String>,
    pub conversation: String,
    pub preview: String,
}

impl SearchHit {
    /// Timestamp/provider/project/conversation/event id, plus a short, scrubbed
    /// excerpt — the shape the CLI prints.
    pub fn line(&self) -> String {
        format!(
            "{}  {}  {}  {}  {}  {}",
            self.timestamp.as_deref().unwrap_or("-"),
            self.provider,
            self.project.as_deref().unwrap_or("-"),
            self.conversation,
            self.event_id,
            self.preview
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub events_indexed: u64,
    pub shards: u64,
    /// Records that were not canonical events (aggregates) or did not parse.
    pub skipped_records: u64,
}

/// Build (or rebuild) the FTS index from the event shards. Idempotent: the
/// previous index, if any, is replaced.
pub fn rebuild_index(dataset: &Path) -> Result<IndexStats> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.is_dataset() {
        bail!("{} is not a dataset", dataset.display());
    }
    let path = index_path(dataset);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Rebuild from scratch so a stale schema can never survive.
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut conn = Connection::open(&path).with_context(|| format!("open {path:?}"))?;
    conn.execute_batch(SCHEMA)?;

    let (shards, files) = shard_files(&layout)?;
    let mut stats = IndexStats {
        events_indexed: 0,
        shards,
        skipped_records: 0,
    };
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare(
            "insert into event_fts(event_id, timestamp, provider, application, project,
                 conversation, text) values (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for file in files {
            let skipped = std::cell::Cell::new(0u64);
            let mut on_error = |_i: u64, _m: String| {
                skipped.set(skipped.get() + 1);
            };
            crate::dataset::read_jsonl_zst::<serde_json::Value>(
                &file,
                MAX_RECORD_BYTES,
                |value| {
                    // Only canonical events are indexed; a shard that also
                    // carries aggregate records contributes just its events.
                    let Ok(event) = serde_json::from_value::<Event>(value) else {
                        skipped.set(skipped.get() + 1);
                        return Ok(());
                    };
                    insert.execute(params![
                        event.event_id,
                        event.timestamp,
                        event.provider,
                        event.application,
                        event.project_id,
                        event.conversation_id,
                        searchable_text(&event),
                    ])?;
                    stats.events_indexed += 1;
                    Ok(())
                },
                &mut on_error,
            )?;
            stats.skipped_records += skipped.get();
        }
    }
    tx.commit()?;
    Ok(stats)
}

/// Everything a search may match on, with the secret policy applied again so an
/// index built from `preserve`-policy data still cannot leak a key.
fn searchable_text(event: &Event) -> String {
    let mut out = String::new();
    for part in &event.content {
        if let Some(t) = part.as_text() {
            out.push_str(&t);
            out.push('\n');
        }
    }
    scrub_for_log(&out)
}

/// The canonical event shards: `data/**/*.jsonl.zst`. Aggregate streams live
/// beside them and are skipped by the event check in `rebuild_index`.
fn shard_files(layout: &Layout) -> Result<(u64, Vec<PathBuf>)> {
    let mut files: Vec<PathBuf> = Vec::new();
    let data = layout.root.join("data");
    if data.is_dir() {
        for entry in walkdir::WalkDir::new(&data)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file()
                && entry.file_name().to_string_lossy().ends_with(".jsonl.zst")
            {
                files.push(entry.path().to_path_buf());
            }
        }
    }
    let shards = files.len() as u64;
    Ok((shards, files))
}

/// Search the index. `query` is treated as whitespace-separated terms that must
/// all match; punctuation cannot reach the FTS5 parser.
pub fn search(dataset: &Path, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let path = index_path(dataset);
    if !path.is_file() {
        bail!(
            "no search index at {} — run rebuild_index() first",
            path.display()
        );
    }
    let expr = match_expression(query);
    if expr.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 1000) as i64;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "select event_id, timestamp, provider, application, project, conversation,
                snippet(event_fts, 6, '', '', ' … ', ?2)
         from event_fts where event_fts match ?1 order by rank limit ?3",
    )?;
    let rows = stmt.query_map(params![expr, SNIPPET_TOKENS as i64, limit], |r| {
        Ok(SearchHit {
            event_id: r.get(0)?,
            timestamp: r.get(1)?,
            provider: r.get(2)?,
            application: r.get(3)?,
            project: r.get(4)?,
            conversation: r.get(5)?,
            preview: preview_of(&r.get::<_, String>(6)?),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Scrub and bound a preview before it can reach a terminal or a report.
fn preview_of(raw: &str) -> String {
    let scrubbed = scrub_for_log(&raw.replace(['\n', '\r'], " "));
    scrubbed.chars().take(PREVIEW_CHARS).collect()
}

/// Turn arbitrary user input into a safe FTS5 expression: every term becomes a
/// quoted phrase, so `NEAR(`, `*` or `"` cannot be interpreted as syntax.
fn match_expression(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let cleaned: String = raw
            .chars()
            .filter(|c| !c.is_control() && *c != '"')
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            continue;
        }
        terms.push(format!("\"{cleaned}\""));
    }
    terms.join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_never_interpreted_as_fts_syntax() {
        assert_eq!(match_expression("hello world"), "\"hello\" AND \"world\"");
        assert_eq!(match_expression("NEAR("), "\"NEAR(\"");
        assert_eq!(match_expression("a\"b"), "\"ab\"");
        assert_eq!(match_expression("  \t "), "");
        assert_eq!(match_expression("*"), "\"*\"");
    }

    #[test]
    fn index_path_is_derived_and_rebuildable() {
        let p = index_path(Path::new("/tmp/ds"));
        assert_eq!(p, PathBuf::from("/tmp/ds/indexes/search.sqlite"));
    }

    #[test]
    fn preview_is_bounded_and_scrubbed() {
        let raw = format!(
            "{}\n{}",
            "x".repeat(1000),
            "sk-abcdefghijklmnopqrstuvwxyz012345"
        );
        let p = preview_of(&raw);
        assert!(p.chars().count() <= PREVIEW_CHARS);
        assert!(!p.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
        assert!(!p.contains('\n'));
    }
}
