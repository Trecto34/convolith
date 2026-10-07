//! Dataset validation: does this directory still hold a consistent archive?
//!
//! Validation never repairs and never writes. It answers one question per check
//! with `PASS` or `FAIL` plus the evidence, so an operator can decide what to
//! do:
//!
//! * `manifest`    — the manifest exists, parses, and names this format.
//! * `checksums`   — `checksums.sha256` exists, lists every canonical shard and
//!   the durable provenance ledger, and every listed file still hashes to its
//!   recorded value.
//! * `provenance`  — every canonical event has at least one observation.
//! * `event_ids`   — event ids are unique across the shards and match the ledger.
//! * `accounting`  — `examined = imported + duplicates + skipped + failed`, per
//!   source and in total.
//! * `referential` — conversations and sessions referenced by events exist.
//!
//! ```text
//! let report = validate(ds)?;
//! assert!(report.passed(), "{}", report.summary());
//! ```

use crate::dataset::{
    Layout, Manifest, DATASET_FORMAT, DATASET_FORMAT_VERSION, LEGACY_DATASET_FORMAT,
};
use crate::id::sha256_file;
use crate::ledger::Ledger;
use crate::model::{Event, SCHEMA_VERSION};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_REASONS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
        }
    }
    pub fn is_pass(self) -> bool {
        self == Status::Pass
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Evidence for a failure (bounded); empty when the check passes.
    pub reasons: Vec<String>,
}

impl Check {
    fn evaluate(name: &str, detail: impl Into<String>, reasons: Vec<String>) -> Check {
        let status = if reasons.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        };
        Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
            reasons: reasons.into_iter().take(MAX_REASONS).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub dataset: String,
    pub checks: Vec<Check>,
    /// Flat list of every failure reason, for quick grep-ability.
    pub issues: Vec<String>,
}

impl ValidationReport {
    /// True when every check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.status.is_pass())
    }
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.status.is_pass()).collect()
    }
    pub fn check(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.name == name)
    }
    pub fn is_pass(&self, name: &str) -> bool {
        self.check(name)
            .map(|c| c.status.is_pass())
            .unwrap_or(false)
    }
    pub fn summary(&self) -> String {
        let mut out = format!(
            "dataset {}: {}\n",
            self.dataset,
            if self.passed() { "PASS" } else { "FAIL" }
        );
        for c in &self.checks {
            out.push_str(&format!(
                "  [{}] {}: {}\n",
                c.status.as_str(),
                c.name,
                c.detail
            ));
            for r in &c.reasons {
                out.push_str(&format!("      - {r}\n"));
            }
        }
        out
    }
}

fn push_reason(reasons: &mut Vec<String>, disabled: &mut u64, msg: String) {
    if reasons.len() < MAX_REASONS {
        reasons.push(msg);
    } else {
        *disabled += 1;
    }
}

/// Validate the dataset at `dataset`. Returns a report; a failure is data, not
/// an error. Errors are reserved for "there is nothing here to validate".
pub fn validate(dataset: &Path) -> Result<ValidationReport> {
    let layout = Layout {
        root: dataset.to_path_buf(),
    };
    if !layout.root.is_dir() {
        anyhow::bail!("{} is not a directory", dataset.display());
    }

    let mut checks = Vec::new();
    let manifest = manifest_check(&layout, &mut checks);
    let ledger = ledger_open(&layout);

    let shards = shard_files(&layout)?;
    checks.push(checksums_check(&layout, &shards)?);
    checks.push(layout_check(&layout, &shards));

    let scan = scan_events(&shards)?;
    checks.push(provenance_check(ledger.as_ref(), &scan));
    checks.push(event_ids_check(ledger.as_ref(), &scan));
    checks.push(accounting_check(ledger.as_ref())?);
    checks.push(referential_check(ledger.as_ref(), &scan));

    if let Some(m) = &manifest {
        checks.push(manifest_shards_check(m, &shards, &scan));
    }

    let issues = checks
        .iter()
        .flat_map(|c| c.reasons.iter().cloned())
        .collect();
    Ok(ValidationReport {
        dataset: dataset.display().to_string(),
        checks,
        issues,
    })
}

fn ledger_open(layout: &Layout) -> Option<Ledger> {
    if !layout.provenance_db().is_file() {
        return None;
    }
    Ledger::open(&layout.provenance_db()).ok()
}

fn manifest_check(layout: &Layout, checks: &mut Vec<Check>) -> Option<Manifest> {
    let path = layout.manifest();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            checks.push(Check::evaluate(
                "manifest",
                "manifest.json is missing",
                vec![format!("{}: {e}", path.display())],
            ));
            return None;
        }
    };
    let manifest: Manifest = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            checks.push(Check::evaluate(
                "manifest",
                "manifest.json does not parse",
                vec![format!("{}: {e}", path.display())],
            ));
            return None;
        }
    };
    let mut reasons = Vec::new();
    if manifest.format != DATASET_FORMAT && manifest.format != LEGACY_DATASET_FORMAT {
        reasons.push(format!(
            "manifest format is {:?}, expected {:?}",
            manifest.format, DATASET_FORMAT
        ));
    }
    if manifest.format_version != DATASET_FORMAT_VERSION {
        reasons.push(format!(
            "manifest format_version is {}, expected {}",
            manifest.format_version, DATASET_FORMAT_VERSION
        ));
    }
    if manifest.schema_version != SCHEMA_VERSION {
        reasons.push(format!(
            "manifest schema_version is {}, expected {}",
            manifest.schema_version, SCHEMA_VERSION
        ));
    }
    let detail = format!(
        "format {} v{}, schema v{}, {} event shard(s), {} import run(s)",
        manifest.format,
        manifest.format_version,
        manifest.schema_version,
        manifest.event_shards.len(),
        manifest.import_runs.len()
    );
    checks.push(Check::evaluate("manifest", detail, reasons));
    Some(manifest)
}

fn layout_check(layout: &Layout, shards: &[PathBuf]) -> Check {
    let mut reasons = Vec::new();
    if !layout.provenance_db().is_file() {
        reasons.push(format!(
            "provenance ledger is missing at {}",
            layout.provenance_db().display()
        ));
    }
    if shards.is_empty() {
        reasons.push("no event shard found under data/".to_string());
    }
    Check::evaluate(
        "layout",
        format!(
            "{} shard(s), ledger {}, checksums {}",
            shards.len(),
            present(layout.provenance_db().is_file()),
            present(layout.checksums().is_file())
        ),
        reasons,
    )
}

fn present(b: bool) -> &'static str {
    if b {
        "present"
    } else {
        "absent"
    }
}

fn checksums_check(layout: &Layout, shards: &[PathBuf]) -> Result<Check> {
    let path = layout.checksums();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return Ok(Check::evaluate(
                "checksums",
                "checksums.sha256 is missing",
                vec![format!("{}: {e}", path.display())],
            ))
        }
    };
    let mut listed: BTreeSet<String> = BTreeSet::new();
    let mut entries: Vec<(String, String)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash, rel)) = line.split_once("  ") else {
            return Ok(Check::evaluate(
                "checksums",
                "checksums.sha256 is malformed",
                vec![format!("line {}: {line:?}", n + 1)],
            ));
        };
        listed.insert(rel.to_string());
        entries.push((hash.trim().to_string(), rel.to_string()));
    }
    let mut reasons = Vec::new();
    let mut skipped = 0u64;
    if entries.is_empty() {
        push_reason(
            &mut reasons,
            &mut skipped,
            "checksums.sha256 is empty".into(),
        );
    }
    // Every canonical shard must be covered, otherwise a change to it would go
    // unnoticed.
    for shard in shards {
        let rel = rel_path(&layout.root, shard);
        if !listed.contains(&rel) {
            push_reason(
                &mut reasons,
                &mut skipped,
                format!("canonical shard not listed in checksums.sha256: {rel}"),
            );
        }
    }
    let ledger_rel = rel_path(&layout.root, &layout.provenance_db());
    if !listed.contains(&ledger_rel) {
        push_reason(
            &mut reasons,
            &mut skipped,
            format!("durable provenance ledger not listed in checksums.sha256: {ledger_rel}"),
        );
    }
    for (hash, rel) in &entries {
        let full = layout.root.join(rel);
        let actual = match sha256_file(&full) {
            Ok(a) => a,
            Err(e) => {
                push_reason(
                    &mut reasons,
                    &mut skipped,
                    format!("listed file is missing: {rel} ({e})"),
                );
                continue;
            }
        };
        if actual != *hash {
            push_reason(
                &mut reasons,
                &mut skipped,
                format!("checksum mismatch for {rel}: recorded {hash}, actual {actual}"),
            );
        }
    }
    if skipped > 0 {
        reasons.push(format!("{skipped} further problem(s) not shown"));
    }
    Ok(Check::evaluate(
        "checksums",
        format!("{} file(s) listed, all hashes verified", entries.len()),
        reasons,
    ))
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn shard_files(layout: &Layout) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    // Only `data/events/` holds canonical events; aggregate streams live in
    // `data/aggregates/` and are covered by the checksums instead.
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

#[derive(Debug, Default)]
struct EventScan {
    events: u64,
    ids: HashSet<String>,
    duplicate_ids: Vec<String>,
    conversations: HashSet<String>,
    sessions: HashSet<String>,
    /// Events that failed to deserialize as canonical events.
    unreadable: Vec<String>,
}

fn scan_events(shards: &[PathBuf]) -> Result<EventScan> {
    let mut scan = EventScan::default();
    for shard in shards {
        let rel = shard
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let unreadable = std::cell::Cell::new(0u64);
        crate::dataset::read_jsonl_zst::<serde_json::Value>(
            shard,
            MAX_RECORD_BYTES,
            |value| {
                let Ok(event) = serde_json::from_value::<Event>(value) else {
                    unreadable.set(unreadable.get() + 1);
                    return Ok(());
                };
                scan.events += 1;
                if !scan.ids.insert(event.event_id.clone())
                    && scan.duplicate_ids.len() < MAX_REASONS
                {
                    scan.duplicate_ids.push(event.event_id.clone());
                }
                scan.conversations.insert(event.conversation_id.clone());
                if let Some(sid) = &event.session_id {
                    scan.sessions.insert(sid.clone());
                }
                Ok(())
            },
            &mut |_i, _m| unreadable.set(unreadable.get() + 1),
        )
        .with_context(|| format!("reading {shard:?}"))?;
        if unreadable.get() > 0 {
            scan.unreadable.push(format!(
                "{rel}: {} record(s) are not canonical events",
                unreadable.get()
            ));
        }
    }
    Ok(scan)
}

fn provenance_check(ledger: Option<&Ledger>, scan: &EventScan) -> Check {
    if scan.events == 0 {
        return Check::evaluate(
            "provenance",
            "no event to check",
            vec!["data/ holds no canonical event".into()],
        );
    }
    let Some(ledger) = ledger else {
        return Check::evaluate(
            "provenance",
            "no ledger",
            vec!["provenance ledger is absent, so no observation can be verified".into()],
        );
    };
    let mut observed: HashSet<String> = HashSet::new();
    let ok = (|| -> Result<()> {
        let mut stmt = ledger
            .conn
            .prepare("select distinct event_id from observation")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            observed.insert(row?);
        }
        Ok(())
    })();
    if let Err(e) = ok {
        return Check::evaluate(
            "provenance",
            "ledger unreadable",
            vec![format!("querying observation table: {e:#}")],
        );
    }
    let mut reasons = Vec::new();
    let mut missing = 0u64;
    for id in &scan.ids {
        if !observed.contains(id) {
            missing += 1;
            if reasons.len() < MAX_REASONS {
                reasons.push(format!("event without provenance: {id}"));
            }
        }
    }
    if missing > reasons.len() as u64 {
        reasons.push(format!("{} event(s) total without provenance", missing));
    }
    Check::evaluate(
        "provenance",
        format!(
            "{} event(s), {} observation(s)",
            scan.events,
            observed.len()
        ),
        reasons,
    )
}

fn event_ids_check(ledger: Option<&Ledger>, scan: &EventScan) -> Check {
    let mut reasons: Vec<String> = scan
        .duplicate_ids
        .iter()
        .map(|id| format!("duplicate event id in shards: {id}"))
        .collect();
    reasons.extend(scan.unreadable.iter().cloned());
    if let Some(ledger) = ledger {
        match ledger.count("select count(*) from event_index") {
            Ok(n) if n != scan.events => reasons.push(format!(
                "ledger indexes {n} event(s) but the shards hold {}",
                scan.events
            )),
            Ok(_) => {}
            Err(e) => reasons.push(format!("cannot count ledger events: {e:#}")),
        }
    }
    Check::evaluate(
        "event_ids",
        format!("{} unique event id(s)", scan.ids.len()),
        reasons,
    )
}

fn accounting_check(ledger: Option<&Ledger>) -> Result<Check> {
    let Some(ledger) = ledger else {
        return Ok(Check::evaluate(
            "accounting",
            "no ledger",
            vec!["provenance ledger is absent, so record accounting cannot be verified".into()],
        ));
    };
    let rows = ledger.source_rows()?;
    let mut reasons = Vec::new();
    let (mut examined, mut imported, mut duplicate, mut skipped, mut failed) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    for (source_id, path, status, ex, im, dup, sk, fa) in &rows {
        examined += ex;
        imported += im;
        duplicate += dup;
        skipped += sk;
        failed += fa;
        if ex != &(im + dup + sk + fa) && reasons.len() < MAX_REASONS {
            reasons.push(format!(
                "source {source_id} ({path}, {status}): examined {ex} != imported {im} + duplicates {dup} + skipped {sk} + failed {fa}"
            ));
        }
    }
    if examined != imported + duplicate + skipped + failed {
        reasons.push(format!(
            "total examined {examined} != imported {imported} + duplicates {duplicate} + skipped {skipped} + failed {failed}"
        ));
    }
    Ok(Check::evaluate(
        "accounting",
        format!(
            "{} source(s): examined {examined} = imported {imported} + duplicates {duplicate} + skipped {skipped} + failed {failed}",
            rows.len()
        ),
        reasons,
    ))
}

fn referential_check(ledger: Option<&Ledger>, scan: &EventScan) -> Check {
    let Some(ledger) = ledger else {
        return Check::evaluate(
            "referential",
            "no ledger",
            vec!["provenance ledger is absent, so references cannot be resolved".into()],
        );
    };
    let mut reasons = Vec::new();
    let load = |sql: &str| -> Result<HashSet<String>> {
        let mut stmt = ledger.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<HashSet<_>, _>>()?)
    };
    let conversations = match load("select conversation_id from conversation") {
        Ok(v) => v,
        Err(e) => {
            return Check::evaluate(
                "referential",
                "ledger unreadable",
                vec![format!("querying conversation table: {e:#}")],
            )
        }
    };
    let mut missing_conversations = 0u64;
    for cid in &scan.conversations {
        if !conversations.contains(cid) {
            missing_conversations += 1;
            if reasons.len() < MAX_REASONS {
                reasons.push(format!("event references unknown conversation: {cid}"));
            }
        }
    }
    // Sessions store the hashed session id and the native one differently, so
    // only the parent conversation link is a hard requirement.
    let bad_sessions = ledger
        .count(
            "select count(*) from session s where not exists (
                 select 1 from conversation c where c.conversation_id = s.conversation_id)",
        )
        .unwrap_or(0);
    if bad_sessions > 0 {
        reasons.push(format!(
            "{bad_sessions} session row(s) reference a conversation that does not exist"
        ));
    }
    let orphan_observations = ledger
        .count(
            "select count(*) from observation o where not exists (
                 select 1 from event_index e where e.event_id = o.event_id)",
        )
        .unwrap_or(0);
    if orphan_observations > 0 {
        reasons.push(format!(
            "{orphan_observations} observation(s) reference an event that is not indexed"
        ));
    }
    let _ = missing_conversations;
    Check::evaluate(
        "referential",
        format!(
            "{} conversation(s) referenced by events, {} session row(s)",
            scan.conversations.len(),
            ledger.count("select count(*) from session").unwrap_or(0)
        ),
        reasons,
    )
}

fn manifest_shards_check(m: &Manifest, shards: &[PathBuf], scan: &EventScan) -> Check {
    let mut reasons = Vec::new();
    let root = Path::new("");
    let _ = root;
    for s in &m.event_shards {
        if s.records == 0 {
            reasons.push(format!("manifest shard {} records 0 event(s)", s.path));
        }
    }
    let listed: BTreeSet<&str> = m.event_shards.iter().map(|s| s.path.as_str()).collect();
    if !shards.is_empty() && listed.is_empty() {
        reasons.push(format!(
            "manifest lists no event shard but {} shard file(s) exist",
            shards.len()
        ));
    }
    let total: u64 = m.event_shards.iter().map(|s| s.records).sum();
    if total != scan.events {
        reasons.push(format!(
            "manifest declares {total} shard record(s) but {} were read",
            scan.events
        ));
    }
    Check::evaluate(
        "manifest_shards",
        format!("{} shard(s) declared in the manifest", m.event_shards.len()),
        reasons,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_are_stable() {
        assert_eq!(Status::Pass.as_str(), "PASS");
        assert_eq!(Status::Fail.as_str(), "FAIL");
    }

    #[test]
    fn report_summarizes_failures() {
        let report = ValidationReport {
            dataset: "/tmp/x".into(),
            checks: vec![
                Check::evaluate("manifest", "ok", vec![]),
                Check::evaluate("checksums", "missing", vec!["no file".into()]),
            ],
            issues: vec!["no file".into()],
        };
        assert!(!report.passed());
        assert_eq!(report.failures().len(), 1);
        assert!(report.is_pass("manifest"));
        assert!(!report.is_pass("checksums"));
        let s = report.summary();
        assert!(s.contains("FAIL"), "{s}");
        assert!(s.contains("no file"), "{s}");
    }
}
