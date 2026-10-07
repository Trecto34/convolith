//! Reports and dataset introspection.
//!
//! Everything here is generated from the dataset itself — the ledger, the
//! manifest and the canonical shards. Nothing is invented: a count that cannot
//! be established is reported as zero with a note saying why, and no report
//! contains a conversation body.
//!
//! ```text
//! stats(ds)                  # real numbers straight from the ledger
//! generate(ds)               # reports/*.md + reports/SOURCE_INVENTORY.{json,md}
//! provenance(ds, event_id)   # where did this event come from?
//! inspect_event(ds, id)      # the canonical record itself
//! ```

use crate::dataset::{Layout, Manifest};
use crate::ledger::{Ledger, ProvenanceRow};
use crate::model::Event;
use crate::secrets::scrub_for_log;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const IMPORT_REPORT: &str = "IMPORT_REPORT.md";
pub const SOURCE_COVERAGE: &str = "SOURCE_COVERAGE.md";
pub const DUPLICATES: &str = "DUPLICATES.md";
pub const CONFLICTS: &str = "CONFLICTS.md";
pub const PARSE_ERRORS: &str = "PARSE_ERRORS.md";
pub const DATA_QUALITY: &str = "DATA_QUALITY.md";
pub const PRIVACY_AUDIT: &str = "PRIVACY_AUDIT.md";
pub const SOURCE_INVENTORY_JSON: &str = "SOURCE_INVENTORY.json";
pub const SOURCE_INVENTORY_MD: &str = "SOURCE_INVENTORY.md";

const MAX_LISTED_ROWS: usize = 500;
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Real dataset numbers. Every field is either read from the ledger, read from
/// the manifest, or counted by scanning the canonical shards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatasetStats {
    pub sources: u64,
    pub sources_parsed: u64,
    pub sources_partial: u64,
    pub sources_unsupported: u64,
    pub sources_skipped: u64,
    pub sources_failed: u64,
    pub conversations: u64,
    pub sessions: u64,
    pub projects: u64,
    pub machines: u64,
    pub events_indexed: u64,
    pub events_in_shards: u64,
    pub tool_calls: u64,
    pub artifacts: u64,
    pub observations: u64,
    pub conflicts: u64,
    pub parse_errors: u64,
    /// Events whose canonical record carries at least one redaction.
    pub events_redacted: u64,
    pub redaction_kinds: BTreeMap<String, u64>,
    pub records_examined: u64,
    pub records_imported: u64,
    pub records_duplicate: u64,
    pub records_skipped: u64,
    pub records_failed: u64,
    /// Times the same canonical event was observed in more than one source.
    pub multi_observed_events: u64,
    pub shards: u64,
    pub shard_bytes: u64,
    pub date_range: Option<[String; 2]>,
    pub ledger_present: bool,
    pub checksums_present: bool,
    /// Sources where `examined != imported + duplicate + skipped + failed`.
    pub accounting_violations: u64,
}

impl DatasetStats {
    pub fn accounting_holds(&self) -> bool {
        self.accounting_violations == 0
            && self.records_examined
                == self.records_imported
                    + self.records_duplicate
                    + self.records_skipped
                    + self.records_failed
    }

    /// Events the ledger claims but the shards do not contain, and vice versa.
    pub fn ledger_in_sync(&self) -> bool {
        self.ledger_present && self.events_indexed == self.events_in_shards
    }
}

/// Read the dataset's numbers. The provenance database is optional; without it
/// shard-derived facts are still reported and everything else is zero.
pub fn stats(dataset: &Path) -> Result<DatasetStats> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.is_dataset() {
        bail!("{} is not a dataset", dataset.display());
    }
    let mut s = DatasetStats {
        checksums_present: layout.checksums().is_file(),
        ledger_present: layout.provenance_db().is_file(),
        ..Default::default()
    };
    let (shards, shard_bytes, events, tool_calls, redacted, kinds) = scan_shards(&layout)?;
    s.shards = shards;
    s.shard_bytes = shard_bytes;
    s.events_in_shards = events;
    s.tool_calls = tool_calls;
    s.events_redacted = redacted;
    s.redaction_kinds = kinds;

    if s.ledger_present {
        let ledger = Ledger::open(&layout.provenance_db())?;
        s.sources = ledger.count("select count(*) from source")?;
        s.sources_parsed = ledger.count("select count(*) from source where status='parsed'")?;
        s.sources_partial =
            ledger.count("select count(*) from source where status='partially_parsed'")?;
        s.sources_unsupported =
            ledger.count("select count(*) from source where status='unsupported'")?;
        s.sources_skipped =
            ledger.count("select count(*) from source where status='skipped_unchanged'")?;
        s.sources_failed = ledger.count("select count(*) from source where status='failed'")?;
        s.conversations = ledger.count("select count(*) from conversation")?;
        s.sessions = ledger.count("select count(*) from session")?;
        s.projects = ledger.count("select count(*) from project")?;
        s.machines = ledger.count("select count(*) from machine")?;
        s.events_indexed = ledger.count("select count(*) from event_index")?;
        s.artifacts = ledger.count("select count(*) from artifact_ref")?;
        s.observations = ledger.count("select count(*) from observation")?;
        s.conflicts = ledger.count("select count(*) from conflict")?;
        s.parse_errors = ledger.count("select count(*) from parse_error")?;
        s.multi_observed_events =
            ledger.count("select count(*) from event_index where observation_count > 1")?;
        s.date_range = ledger.date_range()?.map(|(a, b)| [a, b]);
        for (_, _, _, examined, imported, duplicate, skipped, failed) in ledger.source_rows()? {
            s.records_examined += examined;
            s.records_imported += imported;
            s.records_duplicate += duplicate;
            s.records_skipped += skipped;
            s.records_failed += failed;
            if examined != imported + duplicate + skipped + failed {
                s.accounting_violations += 1;
            }
        }
    } else if let Some(m) = Manifest::load(dataset) {
        // No ledger: the manifest is all we honestly have.
        s.conversations = m.counts.conversations;
        s.events_indexed = m.counts.events;
        s.projects = m.counts.projects;
        s.machines = m.counts.machines;
        s.tool_calls = m.counts.tool_calls;
        s.artifacts = m.counts.artifacts;
        s.conflicts = m.counts.conflicts;
        s.date_range = m.counts.date_range;
    }
    Ok(s)
}

/// Where an event came from. Every observation is returned; an event with no
/// recorded provenance is an error, not an empty list.
pub fn provenance(dataset: &Path, event_id: &str) -> Result<Vec<ProvenanceRow>> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.provenance_db().is_file() {
        bail!(
            "no provenance ledger at {} — cannot resolve {event_id}",
            layout.provenance_db().display()
        );
    }
    let ledger = Ledger::open(&layout.provenance_db())?;
    let rows = ledger.provenance_for(event_id)?;
    if rows.is_empty() {
        bail!("no provenance observation recorded for {event_id}");
    }
    Ok(rows)
}

/// The canonical record for one event id, read back from the shards.
pub fn inspect_event(dataset: &Path, event_id: &str) -> Result<Event> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.is_dataset() {
        bail!("{} is not a dataset", dataset.display());
    }
    let mut found: Option<Event> = None;
    for file in shard_files(&layout)? {
        crate::dataset::read_jsonl_zst::<serde_json::Value>(
            &file,
            MAX_RECORD_BYTES,
            |value| {
                if found.is_some() {
                    return Ok(());
                }
                if value.get("event_id").and_then(|v| v.as_str()) == Some(event_id) {
                    if let Ok(event) = serde_json::from_value::<Event>(value) {
                        found = Some(event);
                    }
                }
                Ok(())
            },
            &mut |_, _| {},
        )?;
        if found.is_some() {
            break;
        }
    }
    found.with_context(|| format!("event {event_id} not found in any canonical shard"))
}

/// What `generate` wrote.
#[derive(Debug, Clone)]
pub struct GeneratedReports {
    pub written: Vec<PathBuf>,
    pub stats: DatasetStats,
}

/// Write every report. Safe to re-run: each file is replaced atomically and no
/// file outside `reports/` is touched (the index and the checksums are the
/// caller's business).
pub fn generate(dataset: &Path) -> Result<GeneratedReports> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.is_dataset() {
        bail!("{} is not a dataset", dataset.display());
    }
    let stats = stats(dataset)?;
    let manifest = Manifest::load(dataset);
    let reports = layout.reports();
    std::fs::create_dir_all(&reports)?;

    let ledger = if layout.provenance_db().is_file() {
        Some(Ledger::open(&layout.provenance_db())?)
    } else {
        None
    };

    let mut written = Vec::new();
    let inventory = inventory_rows(dataset, ledger.as_ref())?;
    let mut write = |name: &str, body: String| -> Result<()> {
        let path = reports.join(name);
        crate::dataset::write_atomic(&path, body.as_bytes())?;
        written.push(path);
        Ok(())
    };

    write(
        IMPORT_REPORT,
        import_report(&layout, &stats, manifest.as_ref()),
    )?;
    write(SOURCE_COVERAGE, source_coverage(&stats, ledger.as_ref())?)?;
    write(DUPLICATES, duplicates_report(&stats, ledger.as_ref())?)?;
    write(CONFLICTS, conflicts_report(&stats, ledger.as_ref())?)?;
    write(PARSE_ERRORS, parse_errors_report(&stats, ledger.as_ref())?)?;
    write(DATA_QUALITY, data_quality(&stats, manifest.as_ref()))?;
    write(PRIVACY_AUDIT, privacy_audit(&stats, manifest.as_ref()))?;
    // Machine-readable inventory first, then the human view of the same rows.
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "tool": "convolith",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "generated_at": crate::timeutil::now_utc().to_rfc3339(),
        "dataset": dataset.display().to_string(),
        "ledger_present": stats.ledger_present,
        "counts": {
            "sources": stats.sources,
            "events": stats.events_in_shards,
            "conversations": stats.conversations,
            "sessions": stats.sessions,
        },
        "sources": inventory,
    }))?;
    write(SOURCE_INVENTORY_JSON, json)?;
    write(SOURCE_INVENTORY_MD, inventory_md(&inventory, &stats))?;

    Ok(GeneratedReports { written, stats })
}

// ---------------------------------------------------------------------------
// Shard scanning
// ---------------------------------------------------------------------------

fn shard_files(layout: &Layout) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let data = layout.events_dir();
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
    Ok(files)
}

type ShardScan = (u64, u64, u64, u64, u64, BTreeMap<String, u64>);

/// Count what the canonical shards actually contain, independent of the ledger.
fn scan_shards(layout: &Layout) -> Result<ShardScan> {
    let files = shard_files(layout)?;
    let mut shards = 0u64;
    let mut bytes = 0u64;
    let mut events = 0u64;
    let mut tool_calls = 0u64;
    let mut redacted = 0u64;
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    for file in &files {
        shards += 1;
        bytes += std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        crate::dataset::read_jsonl_zst::<serde_json::Value>(
            file,
            MAX_RECORD_BYTES,
            |value| {
                let Ok(event) = serde_json::from_value::<Event>(value) else {
                    return Ok(());
                };
                events += 1;
                if matches!(
                    event.event_type,
                    crate::model::EventType::ToolCall | crate::model::EventType::ToolResult
                ) {
                    tool_calls += 1;
                }
                if !event.redactions.is_empty() {
                    redacted += 1;
                    for r in &event.redactions {
                        *kinds.entry(r.kind.clone()).or_insert(0) += r.count as u64;
                    }
                }
                Ok(())
            },
            &mut |_, _| {},
        )?;
    }
    Ok((shards, bytes, events, tool_calls, redacted, kinds))
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// One inventory row per source: the ledger's view, or the importer's JSON when
/// the ledger is absent. Never contains conversation content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub source_id: String,
    pub original_path: String,
    pub status: String,
    pub parser: String,
    pub format: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub records_examined: u64,
    pub records_imported: u64,
    pub records_duplicate: u64,
    pub records_skipped: u64,
    pub records_failed: u64,
}

fn inventory_rows(dataset: &Path, ledger: Option<&Ledger>) -> Result<Vec<InventoryEntry>> {
    if let Some(l) = ledger {
        let mut stmt = l.conn.prepare(
            "select source_id, original_path, status, ifnull(parser,''), ifnull(format,''), size,
                    sha256, records_examined, records_imported, records_duplicate,
                    records_skipped, records_failed
             from source order by original_path, source_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(InventoryEntry {
                source_id: r.get(0)?,
                original_path: r.get(1)?,
                status: r.get(2)?,
                parser: r.get(3)?,
                format: r.get(4)?,
                size: r.get::<_, i64>(5)?.max(0) as u64,
                sha256: r.get(6)?,
                records_examined: r.get::<_, i64>(7)?.max(0) as u64,
                records_imported: r.get::<_, i64>(8)?.max(0) as u64,
                records_duplicate: r.get::<_, i64>(9)?.max(0) as u64,
                records_skipped: r.get::<_, i64>(10)?.max(0) as u64,
                records_failed: r.get::<_, i64>(11)?.max(0) as u64,
            })
        })?;
        return Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    // Fall back to the importer's own inventory, if one was written.
    let path = dataset.join("reports").join("SOURCE_INVENTORY.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut out = Vec::new();
    if let Some(rows) = value.get("sources").and_then(|v| v.as_array()) {
        for row in rows {
            let get_str = |k: &str| {
                row.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let get_u64 = |k: &str| row.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            out.push(InventoryEntry {
                source_id: get_str("source_id"),
                original_path: get_str("original_path"),
                status: get_str("status"),
                parser: get_str("parser"),
                format: get_str("format"),
                size: get_u64("size"),
                sha256: row
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                records_examined: get_u64("records_examined"),
                records_imported: get_u64("records_imported"),
                records_duplicate: get_u64("records_duplicate"),
                records_skipped: get_u64("records_skipped"),
                records_failed: get_u64("records_failed"),
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Report bodies
// ---------------------------------------------------------------------------

fn import_report(layout: &Layout, stats: &DatasetStats, manifest: Option<&Manifest>) -> String {
    let mut s = String::from(
        "# Import report\n\nGenerated from the dataset as it stands on disk. Counts are measured, never estimated.\n\n",
    );
    s.push_str(&format!("Dataset: `{}`\n\n", layout.root.display()));
    match manifest {
        Some(m) => {
            s.push_str(&format!(
                "- format: `{}` v{} (schema v{})\n- tool: {} {}\n- created: {}\n- last updated: {}\n- redaction policy: `{}`\n\n",
                m.format, m.format_version, m.schema_version, m.tool, m.tool_version, m.created_at, m.updated_at, m.redaction_policy
            ));
        }
        None => s.push_str("No `manifest.json` was readable; only on-disk facts are reported.\n\n"),
    }
    s.push_str("| metric | value |\n|---|---:|\n");
    for (k, v) in [
        ("sources", stats.sources),
        ("  parsed", stats.sources_parsed),
        ("  partially parsed", stats.sources_partial),
        ("  unsupported", stats.sources_unsupported),
        ("  skipped (unchanged)", stats.sources_skipped),
        ("  failed", stats.sources_failed),
        ("conversations", stats.conversations),
        ("sessions", stats.sessions),
        ("projects", stats.projects),
        ("machines", stats.machines),
        ("events canonical (shards)", stats.events_in_shards),
        ("events indexed (ledger)", stats.events_indexed),
        ("tool calls/results", stats.tool_calls),
        ("artifacts", stats.artifacts),
        ("provenance observations", stats.observations),
        ("conflicts", stats.conflicts),
        ("parse errors", stats.parse_errors),
        ("events with redactions", stats.events_redacted),
        ("event shards", stats.shards),
    ] {
        s.push_str(&format!("| {k} | {v} |\n"));
    }
    if let Some([a, b]) = &stats.date_range {
        s.push_str(&format!("| date range | {a} … {b} |\n"));
    }
    s.push_str(&format!("| shard bytes | {} |\n", stats.shard_bytes));

    s.push_str("\n## Record accounting\n\n");
    s.push_str("Per-source invariant: `examined = imported + duplicates + skipped + failed`.\n\n");
    s.push_str("| bucket | records |\n|---|---:|\n");
    for (k, v) in [
        ("examined", stats.records_examined),
        ("imported", stats.records_imported),
        ("duplicates", stats.records_duplicate),
        ("skipped", stats.records_skipped),
        ("failed", stats.records_failed),
    ] {
        s.push_str(&format!("| {k} | {v} |\n"));
    }
    s.push_str(&format!(
        "\nSources violating the identity: **{}**. Verdict: {}.\n",
        stats.accounting_violations,
        if stats.accounting_holds() {
            "holds"
        } else {
            "does not hold"
        }
    ));
    s.push_str(&format!(
        "\nLedger agrees with the shards on event count: {} ({} indexed vs {} in shards).\n",
        if stats.ledger_in_sync() { "yes" } else { "no" },
        stats.events_indexed,
        stats.events_in_shards
    ));
    if manifest.map(|m| !m.import_runs.is_empty()).unwrap_or(false) {
        let m = manifest.unwrap();
        s.push_str("\n## Import runs\n\n| run | started | finished | state | new | duplicate | sources examined | failed |\n|---|---|---|---|---:|---:|---:|---:|\n");
        for r in &m.import_runs {
            s.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                r.import_run,
                r.started_at,
                r.finished_at.clone().unwrap_or_default(),
                r.state,
                r.events_new,
                r.events_duplicate,
                r.sources_examined,
                r.sources_failed
            ));
        }
    }
    s
}

fn source_coverage(stats: &DatasetStats, ledger: Option<&Ledger>) -> Result<String> {
    let mut s = String::from(
        "# Source coverage\n\nWhat was found, what was understood, and what was left unresolved. Unsupported files are reported, never guessed at.\n\n",
    );
    s.push_str("| status | sources |\n|---|---:|\n");
    for (k, v) in [
        ("parsed", stats.sources_parsed),
        ("partially_parsed", stats.sources_partial),
        ("unsupported", stats.sources_unsupported),
        ("skipped_unchanged", stats.sources_skipped),
        ("failed", stats.sources_failed),
    ] {
        s.push_str(&format!("| {k} | {v} |\n"));
    }
    let Some(ledger) = ledger else {
        s.push_str(
            "\nNo provenance ledger is present, so per-parser coverage cannot be measured.\n",
        );
        return Ok(s);
    };
    if stats.sources == 0 {
        s.push_str("\nNo sources are recorded in the ledger.\n");
        return Ok(s);
    }
    s.push_str("\n## Coverage by parser\n\n| parser | format | sources | examined | imported | duplicates | skipped | failed |\n|---|---|---:|---:|---:|---:|---:|---:|\n");
    let mut stmt = ledger.conn.prepare(
        "select ifnull(parser,'') as p, ifnull(format,'') as f, count(*),
                sum(records_examined), sum(records_imported), sum(records_duplicate),
                sum(records_skipped), sum(records_failed)
         from source group by p, f order by count(*) desc, p",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?.max(0) as u64,
            r.get::<_, i64>(3)?.max(0) as u64,
            r.get::<_, i64>(4)?.max(0) as u64,
            r.get::<_, i64>(5)?.max(0) as u64,
            r.get::<_, i64>(6)?.max(0) as u64,
            r.get::<_, i64>(7)?.max(0) as u64,
        ))
    })?;
    for row in rows {
        let (p, f, n, ex, im, dup, sk, fa) = row?;
        let name = if p.is_empty() { "_(none)_".into() } else { p };
        let fmt = if f.is_empty() {
            "_(unknown)_".into()
        } else {
            f
        };
        s.push_str(&format!(
            "| `{name}` | {fmt} | {n} | {ex} | {im} | {dup} | {sk} | {fa} |\n"
        ));
    }
    if stats.sources_unsupported > 0 {
        s.push_str("\n## Unresolved files\n\nNo parser claimed these files. Their bytes were not interpreted.\n\n| path |\n|---|\n");
        let mut stmt = ledger.conn.prepare(
            "select original_path from source where status='unsupported' order by original_path limit ?1",
        )?;
        let rows = stmt.query_map([MAX_LISTED_ROWS as i64], |r| r.get::<_, String>(0))?;
        for row in rows {
            s.push_str(&format!("| `{}` |\n", one_line(&row?)));
        }
    }
    Ok(s)
}

fn duplicates_report(stats: &DatasetStats, ledger: Option<&Ledger>) -> Result<String> {
    let mut s = String::from(
        "# Duplicates\n\nThe same canonical event seen in more than one place. Duplicates are collapsed, never deleted: every observation stays in the ledger.\n\n",
    );
    s.push_str(&format!(
        "Observations: **{}** across **{}** canonical events. Events observed more than once: **{}**. Duplicate records counted during import: **{}**.\n",
        stats.observations, stats.events_indexed, stats.multi_observed_events, stats.records_duplicate
    ));
    let Some(ledger) = ledger else {
        return Ok(s);
    };
    let rows = ledger.multi_observed(MAX_LISTED_ROWS as u64)?;
    if rows.is_empty() {
        s.push_str("\nNo event was observed more than once.\n");
        return Ok(s);
    }
    s.push_str(
        "\n## Most corroborated events\n\n| event | observations | sources |\n|---|---:|---|\n",
    );
    for (id, n, sources) in rows {
        s.push_str(&format!("| `{id}` | {n} | {} |\n", one_line(&sources)));
    }
    Ok(s)
}

fn conflicts_report(stats: &DatasetStats, ledger: Option<&Ledger>) -> Result<String> {
    let mut s = String::from(
        "# Conflicts\n\nSame canonical identity, different content. Both variants are preserved; no winner is chosen silently.\n\n",
    );
    s.push_str(&format!("Conflicts recorded: **{}**.\n", stats.conflicts));
    let Some(ledger) = ledger else {
        return Ok(s);
    };
    let rows = ledger.conflicts()?;
    if rows.is_empty() {
        s.push_str("\nNo conflicts were recorded.\n");
        return Ok(s);
    }
    s.push_str("\n| event | variant | kind | detail | source |\n|---|---|---|---|---|\n");
    for (event, variant, kind, detail, source) in rows.iter().take(MAX_LISTED_ROWS) {
        s.push_str(&format!(
            "| `{event}` | `{variant}` | {kind} | {} | `{}` |\n",
            one_line(detail),
            one_line(source)
        ));
    }
    if rows.len() > MAX_LISTED_ROWS {
        s.push_str(&format!(
            "\n{} further conflict(s) omitted.\n",
            rows.len() - MAX_LISTED_ROWS
        ));
    }
    Ok(s)
}

fn parse_errors_report(stats: &DatasetStats, ledger: Option<&Ledger>) -> Result<String> {
    let mut s = String::from(
        "# Parse errors\n\nA damaged source never stops the import; every failure is listed here. Messages are scrubbed of secret-like material.\n\n",
    );
    s.push_str(&format!(
        "Parse error records: **{}**.\n",
        stats.parse_errors
    ));
    let Some(ledger) = ledger else {
        return Ok(s);
    };
    let errs = ledger.parse_errors()?;
    if errs.is_empty() {
        s.push_str("\nNo parse errors were recorded.\n");
        return Ok(s);
    }
    s.push_str("\n| source | locator | message |\n|---|---|---|\n");
    for (src, loc, msg) in errs.iter().take(MAX_LISTED_ROWS) {
        s.push_str(&format!(
            "| `{}` | {} | {} |\n",
            one_line(src),
            one_line(&loc.clone().unwrap_or_default()),
            one_line(&scrub_for_log(msg))
        ));
    }
    Ok(s)
}

fn data_quality(stats: &DatasetStats, manifest: Option<&Manifest>) -> String {
    let mut s = String::from(
        "# Data quality\n\nWhat is known, what is approximate, and what is missing. Nothing here is inferred beyond the evidence.\n\n",
    );
    s.push_str("| observation | value |\n|---|---:|\n");
    s.push_str(&format!("| sources recorded | {} |\n", stats.sources));
    s.push_str(&format!(
        "| sources parsed cleanly | {} |\n",
        stats.sources_parsed
    ));
    s.push_str(&format!(
        "| sources partially parsed | {} |\n",
        stats.sources_partial
    ));
    s.push_str(&format!(
        "| sources with unknown format | {} |\n",
        stats.sources_unsupported
    ));
    s.push_str(&format!("| sources failed | {} |\n", stats.sources_failed));
    s.push_str(&format!("| parse errors | {} |\n", stats.parse_errors));
    s.push_str(&format!(
        "| events with redactions | {} |\n",
        stats.events_redacted
    ));
    s.push_str(&format!(
        "| conflicts (unresolved differences) | {} |\n",
        stats.conflicts
    ));
    s.push_str(&format!(
        "| events observed more than once | {} |\n",
        stats.multi_observed_events
    ));
    s.push_str(&format!(
        "| provenance ledger present | {} |\n",
        yes_no(stats.ledger_present)
    ));
    s.push_str(&format!(
        "| checksums.sha256 present | {} |\n",
        yes_no(stats.checksums_present)
    ));
    s.push_str(&format!(
        "| record accounting holds | {} |\n",
        yes_no(stats.accounting_holds())
    ));
    s.push_str(&format!(
        "| ledger matches shard event count | {} |\n",
        yes_no(stats.ledger_in_sync())
    ));
    if let Some(m) = manifest {
        s.push_str(&format!(
            "| cross-machine provenance declared | see manifest counts ({} machines) |\n",
            m.counts.machines
        ));
    }
    s.push_str("\n## Limits of this dataset\n\n");
    s.push_str("- Events are canonical reconstructions; a parser that understood only part of a format says so via `partially_parsed`.\n");
    s.push_str(
        "- Timestamps carry their own confidence (see `timestamp_confidence` on each event).\n",
    );
    s.push_str("- Unsupported files are inventoried but not interpreted.\n");
    s.push_str("- Anything the vendor stored encrypted is kept opaque and is not decoded.\n");
    s
}

fn privacy_audit(stats: &DatasetStats, manifest: Option<&Manifest>) -> String {
    let policy = manifest
        .map(|m| m.redaction_policy.clone())
        .unwrap_or_else(|| "unknown".into());
    let mut s = format!(
        "# Privacy audit\n\nOnly counts appear here. No secret value reaches a report, a log or the manifest.\n\nCanonical redaction policy (from `manifest.json`): `{policy}`.\n\n"
    );
    s.push_str(&format!(
        "Events whose canonical record carries at least one redaction: **{}** of **{}**.\n\n",
        stats.events_redacted, stats.events_in_shards
    ));
    if stats.redaction_kinds.is_empty() {
        s.push_str("No secret-like material was recorded in the canonical data.\n");
    } else {
        s.push_str("| kind | occurrences |\n|---|---:|\n");
        for (k, v) in &stats.redaction_kinds {
            s.push_str(&format!("| {k} | {v} |\n"));
        }
    }
    s.push_str("\n## Guarantees\n\n");
    s.push_str("- Redaction markers replace the value in the canonical text; the original source file is never modified.\n");
    s.push_str("- With policy `preserve`, values stay in the canonical data but are still counted here, and are still scrubbed from logs and reports.\n");
    s.push_str("- The search index stores the same text as the canonical record, passed through the same policy.\n");
    s
}

fn inventory_md(rows: &[InventoryEntry], stats: &DatasetStats) -> String {
    let mut s = String::from(
        "# Source inventory\n\nOne row per source. Paths, statuses and record counts only — never conversation bodies.\n\n",
    );
    s.push_str(&format!(
        "{} source(s) recorded.\n\n| source | path | status | parser | format | bytes | records (e/i/d/s/f) | sha256 |\n|---|---|---|---|---:|---|---|---|\n",
        rows.len()
    ));
    for r in rows.iter().take(MAX_LISTED_ROWS) {
        s.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} | {} | {}/{}/{}/{}/{} | {} |\n",
            r.source_id,
            one_line(&r.original_path),
            r.status,
            if r.parser.is_empty() { "-" } else { &r.parser },
            if r.format.is_empty() { "-" } else { &r.format },
            r.size,
            r.records_examined,
            r.records_imported,
            r.records_duplicate,
            r.records_skipped,
            r.records_failed,
            r.sha256
                .as_deref()
                .map(|h| format!("`{h}`"))
                .unwrap_or_else(|| "-".into())
        ));
    }
    if rows.len() > MAX_LISTED_ROWS {
        s.push_str(&format!(
            "\n{} further source(s) omitted; the full list is in `SOURCE_INVENTORY.json`.\n",
            rows.len() - MAX_LISTED_ROWS
        ));
    }
    s.push_str(&format!(
        "\nTotals: {} source(s), {} event(s) in shards.\n",
        stats.sources, stats.events_in_shards
    ));
    s
}

fn one_line(s: &str) -> String {
    scrub_for_log(&s.replace('|', "\\|").replace(['\n', '\r'], " "))
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_refuse_a_non_dataset() {
        let d = std::env::temp_dir().join(format!("convolith-nods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(stats(&d).is_err());
        assert!(generate(&d).is_err());
        assert!(inspect_event(&d, "ev_x").is_err());
        assert!(provenance(&d, "ev_x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn report_names_are_stable() {
        assert_eq!(IMPORT_REPORT, "IMPORT_REPORT.md");
        assert_eq!(SOURCE_INVENTORY_JSON, "SOURCE_INVENTORY.json");
    }

    #[test]
    fn markdown_escaping_keeps_tables_intact() {
        assert_eq!(one_line("a|b\nc"), "a\\|b c");
    }
}
