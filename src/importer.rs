//! The import pipeline: discovery → detection → parsing → canonicalization →
//! provenance-aware dedup → canonical shards.
//!
//! Central invariant (spec §43): for every source,
//!
//! ```text
//! records_examined = records_imported + records_duplicate + records_skipped + records_failed
//! ```
//!
//! and every discovered file lands in exactly one bucket with a recorded
//! reason. Failures are recorded in `reports/PARSE_ERRORS.md` and in the ledger
//! while the remaining sources continue.
//!
//! Sources are read-only. Anything that needs mutation (a database with a hot
//! WAL, an archive member) is first copied into a tool-owned staging directory.

use crate::artifacts::{guess_mime, ArtifactStore};
use crate::config::Config;
use crate::dataset::{self, ImportRunRecord, Layout, Manifest, ShardRef};
use crate::dedup::{self, IdentityInput};
use crate::discover::{self, DiscoverOptions, DiscoverStats};
use crate::ledger::{EventStatus, Ledger, SourceRecord};
use crate::model::{
    ArtifactRecord, Event, EventDraft, EventType, Machine, Part, Project, ProvenanceRef, Redaction,
    RunStats, Session, Truncation,
};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, Registry};
use crate::scratch::Scratch;
use crate::secrets::{RedactionHit, SecretPolicy, Secrets};
use crate::source::{Detection, Probe, Source};
use crate::timeutil;
use anyhow::Result;
use serde_json::{json, Map};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const INVENTORY_JSON: &str = "SOURCE_INVENTORY.json";

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub output: PathBuf,
    pub dry_run: bool,
    pub resume: bool,
    pub secrets: SecretPolicy,
    pub machine_id: Option<String>,
    pub platform: Option<String>,
    pub keep_staging: bool,
    pub max_record_bytes: usize,
    pub max_inline_text_bytes: usize,
    pub max_artifact_bytes: u64,
    pub max_stage_bytes: u64,
    pub max_files: u64,
    pub max_file_bytes: u64,
    pub max_depth: usize,
    pub skip_extensions: Vec<String>,
    pub expand_archives: bool,
    pub only_parser: Option<String>,
    /// See `DiscoverOptions::path_map`.
    pub path_map: Option<(PathBuf, String)>,
    /// See `DiscoverOptions::known_stores_only`.
    pub known_stores_only: bool,
    pub args: Vec<String>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            output: PathBuf::from("canonical-ai-history"),
            dry_run: false,
            resume: false,
            secrets: SecretPolicy::Redact,
            machine_id: None,
            platform: None,
            keep_staging: false,
            max_record_bytes: 64 * 1024 * 1024,
            max_inline_text_bytes: 32 * 1024,
            max_artifact_bytes: 512 * 1024 * 1024,
            max_stage_bytes: 64 * 1024 * 1024 * 1024,
            max_files: 2_000_000,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            max_depth: 48,
            skip_extensions: crate::config::Limits::default().skip_extensions,
            expand_archives: true,
            only_parser: None,
            path_map: None,
            known_stores_only: false,
            args: Vec::new(),
        }
    }
}

impl ImportOptions {
    pub fn from_limits(mut self, l: &crate::config::Limits) -> Self {
        self.max_record_bytes = l.max_record_bytes;
        self.max_inline_text_bytes = l.max_inline_text_bytes;
        self.max_artifact_bytes = l.max_artifact_bytes;
        self.max_stage_bytes = l.max_stage_bytes;
        self.max_files = l.max_files;
        self.max_file_bytes = l.max_file_bytes;
        self.max_depth = l.max_depth;
        self.skip_extensions = l.skip_extensions.clone();
        self
    }

    pub fn secrets_str(&self) -> &'static str {
        match self.secrets {
            SecretPolicy::Redact => "redact",
            SecretPolicy::Preserve => "preserve",
        }
    }

    pub fn secret_policy(&self) -> SecretPolicy {
        self.secrets
    }
}

/// One inventory row per discovered file, mirroring spec §12. Conversation
/// bodies never appear here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InventoryRow {
    pub source_id: String,
    pub original_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container_chain: Vec<String>,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_guess: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_guess: Option<String>,
    pub parser: String,
    pub detection_confidence: String,
    pub status: String,
    pub records_examined: u64,
    pub records_imported: u64,
    pub records_duplicate: u64,
    pub records_skipped: u64,
    pub records_failed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl InventoryRow {
    /// The ledger row for a source that was inventoried without being parsed,
    /// so the reports (which read the ledger) list it.
    fn to_source_record(&self, run: &str) -> SourceRecord {
        let now = timeutil::now_utc().to_rfc3339();
        SourceRecord {
            source_id: self.source_id.clone(),
            display_path: self.original_path.clone(),
            original_path: self.original_path.clone(),
            container_chain: self.container_chain.clone(),
            size: self.size,
            mtime: self.mtime.clone(),
            sha256: self.sha256.clone(),
            machine_id: self.machine_id.clone(),
            platform: self.platform.clone(),
            provider_guess: self.provider_guess.clone(),
            application_guess: self.application_guess.clone(),
            format: Some(self.format.clone()),
            parser: None,
            parser_version: None,
            detection_confidence: Some(self.detection_confidence.clone()),
            status: self.status.clone(),
            import_run: run.to_string(),
            first_seen: now.clone(),
            last_seen: now,
            records_examined: 0,
            records_imported: 0,
            records_duplicate: 0,
            records_skipped: 0,
            records_failed: 0,
            notes: self.note.clone().unwrap_or_default(),
        }
    }
}

/// Mutable state shared by the parse context and the event sink. A parser gets
/// both as separate borrows, so the state itself lives behind `Rc<RefCell<_>>`.
pub struct ImportState {
    pub ledger: Ledger,
    pub artifacts: ArtifactStore,
    pub layout: Layout,
    pub run: String,
    pub dry_run: bool,
    pub counts: RunStats,
    pub inventory: Vec<InventoryRow>,
    pub quality: Vec<String>,
    pub secret_kinds: BTreeMap<String, u64>,
    pub redacted_events: u64,
    pub artifacts_out: Vec<ArtifactRecord>,
    pub shards: Vec<ShardRef>,
    pub writer: Option<dataset::JsonlZstWriter>,
    pub shard_rel: String,
}

impl ImportState {
    fn write_event(&mut self, event: &Event) -> Result<()> {
        if let Some(w) = self.writer.as_mut() {
            w.write_record(event)?;
        }
        Ok(())
    }
}

pub struct Importer {
    opts: ImportOptions,
    registry: Registry,
    cfg: Config,
    layout: Layout,
    secrets: Secrets,
    run: String,
    started_at: String,
    state: Rc<RefCell<ImportState>>,
    scratch: Option<Scratch>,
    progress: Option<indicatif::ProgressBar>,
}

impl Importer {
    pub fn new(opts: ImportOptions, cfg: Config, registry: Registry) -> Result<Importer> {
        let layout = Layout {
            root: opts.output.clone(),
        };
        let secrets = Secrets::new(opts.secrets);
        let started_at = timeutil::now_utc().to_rfc3339();
        let run = dedup::import_run_id(&started_at);
        layout.ensure()?;
        let state = Rc::new(RefCell::new(ImportState {
            ledger: Ledger::open(&layout.provenance_db())?,
            artifacts: ArtifactStore::new(&layout.artifacts(), opts.max_artifact_bytes.max(1)),
            layout: Layout {
                root: opts.output.clone(),
            },
            run: run.clone(),
            dry_run: opts.dry_run,
            counts: RunStats::empty(&run, &started_at),
            inventory: Vec::new(),
            quality: Vec::new(),
            secret_kinds: BTreeMap::new(),
            redacted_events: 0,
            artifacts_out: Vec::new(),
            shards: Vec::new(),
            writer: None,
            shard_rel: String::new(),
        }));
        Ok(Importer {
            opts,
            registry,
            cfg,
            layout,
            secrets,
            run,
            started_at,
            state,
            scratch: None,
            progress: None,
        })
    }

    pub fn set_progress(&mut self, progress: indicatif::ProgressBar) {
        self.progress = Some(progress);
    }

    pub fn with_progress(mut self, progress: indicatif::ProgressBar) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run
    }

    pub fn state(&self) -> Rc<RefCell<ImportState>> {
        self.state.clone()
    }

    pub fn import(&mut self, roots: &[PathBuf]) -> Result<RunStats> {
        let mut scratch = Scratch::create(&self.layout.scratch_parent(), "run")?;
        let staging = scratch.path().to_path_buf();
        if self.opts.keep_staging {
            scratch.keep();
        }

        let mut discover_stats = DiscoverStats::default();
        let mut roots = roots.to_vec();
        roots.sort();
        roots.dedup();
        for root in &roots {
            let dog = DiscoverOptions {
                max_depth: self.opts.max_depth,
                max_files: self.opts.max_files,
                skip_extensions: self.opts.skip_extensions.clone(),
                expand_archives: self.opts.expand_archives,
                follow_symlinks: false,
                max_file_bytes: self.opts.max_file_bytes,
                machine_id: self.opts.machine_id.clone(),
                platform_hint: self.opts.platform.clone(),
                skip_dirs: Vec::new(),
                known_stores_only: self.opts.known_stores_only,
                path_map: self.opts.path_map.clone(),
            };
            let staging_cb = staging.clone();
            let state = self.state.clone();
            let registry = &self.registry;
            let opts = self.opts.clone();
            let secrets = self.secrets.clone_for_thread();
            let progress = self.progress.clone();
            let mut err: Option<anyhow::Error> = None;
            {
                let mut on_source = |source: &Source, probe: &Probe| -> Result<()> {
                    let display = source.display_path.clone();
                    if let Some(pb) = &progress {
                        pb.set_message(format!("importing {display}"));
                    }
                    let res = import_source(
                        registry,
                        &state,
                        &staging_cb,
                        source,
                        probe,
                        &opts,
                        &secrets,
                        &self.cfg,
                        self.run.clone(),
                    );
                    if let Some(pb) = &progress {
                        crate::progress::inc_progress(pb);
                        let st = state.borrow();
                        let events = st.counts.records_imported + st.counts.records_duplicate;
                        pb.set_message(format!("{display} ({events} events)"));
                    }
                    res
                };
                if let Err(e) = discover::walk_root(
                    root,
                    &dog,
                    &self.cfg,
                    &staging,
                    &mut on_source,
                    &mut discover_stats,
                ) {
                    err = Some(e.context(format!("discovering {}", root.display())));
                }
            }
            if let Some(e) = err {
                discover_stats.errors.push(format!("{e:#}"));
            }
            // A root is a durability unit: everything parsed so far is committed.
            self.state.borrow_mut().ledger.flush()?;
        }

        if let Some(pb) = &self.progress {
            pb.set_message("writing dataset aggregates and inventory...");
        }

        self.close_shard()?;
        self.record_failed_sources(&discover_stats)?;
        self.record_unresolved_sources(&discover_stats)?;

        let mut summary = {
            let mut st = self.state.borrow_mut();
            st.counts.files_discovered = discover_stats.files_seen;
            st.counts.files_skipped += discover_stats.files_skipped;
            st.counts.sources_failed += discover_stats.failed_sources.len() as u64;
            st.counts.bytes_hashed = discover_stats.bytes_seen;
            st.counts.candidate_sources = st.counts.supported_sources
                + st.counts.unsupported_sources
                + st.counts.sources_failed;
            st.counts.clone()
        };
        summary.finished_at = Some(timeutil::now_utc().to_rfc3339());

        if !summary.accounting_holds() {
            anyhow::bail!("record accounting invariant violated: {summary:?}");
        }

        if !self.opts.dry_run {
            self.write_aggregates(&summary)?;
            self.write_manifest(&summary, &discover_stats)?;
        }
        self.write_inventory(&discover_stats, &summary)?;

        {
            let mut st = self.state.borrow_mut();
            st.ledger.finish()?;
        }
        if self.opts.keep_staging {
            eprintln!("convolith: staging kept at {}", staging.display());
        } else {
            self.scratch = Some(scratch);
        }
        Ok(summary)
    }

    /// Sources the walk itself could not read (corrupt archive or entry) are
    /// recorded in the ledger so reports and `validate` see them.
    /// Sources discovery found but deliberately did not interpret (LevelDB
    /// stores without a decoder): inventoried as unsupported with an "unresolved" note, never guessed at.
    fn record_unresolved_sources(&mut self, d: &DiscoverStats) -> Result<()> {
        let mut st = self.state.borrow_mut();
        let run = st.run.clone();
        for u in &d.unresolved_sources {
            let text = |k: &str| u[k].as_str().unwrap_or_default().to_string();
            let (id, path) = (text("source_id"), text("display_path"));
            let note = crate::secrets::scrub_for_log(&text("note"));
            let size = u["size"].as_u64().unwrap_or(0);
            st.counts.unsupported_sources += 1;
            let row = InventoryRow {
                source_id: id,
                original_path: path,
                container_chain: Vec::new(),
                size,
                mtime: None,
                sha256: None,
                machine_id: None,
                platform: None,
                format: "leveldb".into(),
                provider_guess: None,
                application_guess: None,
                parser: String::new(),
                detection_confidence: "none".into(),
                status: "unsupported".into(),
                records_examined: 0,
                records_imported: 0,
                records_duplicate: 0,
                records_skipped: 0,
                records_failed: 0,
                note: Some(note),
            };
            st.ledger.record_source(&row.to_source_record(&run))?;
            st.inventory.push(row);
        }
        Ok(())
    }

    fn record_failed_sources(&mut self, d: &DiscoverStats) -> Result<()> {
        let now = timeutil::now_utc().to_rfc3339();
        let mut st = self.state.borrow_mut();
        let run = st.run.clone();
        for f in &d.failed_sources {
            let text = |k: &str| f[k].as_str().unwrap_or_default().to_string();
            let (id, path, note) = (text("source_id"), text("display_path"), text("note"));
            let note = crate::secrets::scrub_for_log(&note);
            st.ledger.record_source(&SourceRecord {
                source_id: id.clone(),
                display_path: path.clone(),
                original_path: path.clone(),
                container_chain: Vec::new(),
                size: 0,
                mtime: None,
                sha256: None,
                machine_id: None,
                platform: None,
                provider_guess: None,
                application_guess: None,
                format: None,
                parser: None,
                parser_version: None,
                detection_confidence: None,
                status: "failed".into(),
                import_run: run.clone(),
                first_seen: now.clone(),
                last_seen: now.clone(),
                records_examined: 0,
                records_imported: 0,
                records_duplicate: 0,
                records_skipped: 0,
                records_failed: 0,
                notes: note.clone(),
            })?;
            st.ledger
                .record_parse_error(&id, Some(&path), None, &note, &run, &now)?;
            st.counts.parse_errors += 1;
        }
        Ok(())
    }

    fn close_shard(&mut self) -> Result<()> {
        let mut st = self.state.borrow_mut();
        let run = st.run.clone();
        if let Some(w) = st.writer.take() {
            if w.records() == 0 {
                // A run that imported nothing must not add an empty shard.
                drop(w);
                st.shard_rel.clear();
            } else {
                let rel = st.shard_rel.clone();
                let shard = w.finish(&rel, &run)?;
                st.shards.push(shard);
            }
        }
        Ok(())
    }

    fn write_aggregates(&mut self, summary: &RunStats) -> Result<()> {
        let (convs, sessions, projects, machines, artifacts) = {
            let st = self.state.borrow();
            (
                st.ledger.aggregate_conversations()?,
                st.ledger.aggregate_sessions()?,
                st.ledger.aggregate_projects()?,
                st.ledger.aggregate_machines()?,
                st.ledger.aggregate_artifacts()?,
            )
        };
        let agg = self.layout.aggregates_dir();
        let run = self.run.clone();
        write_stream(
            &agg.join("conversations.jsonl.zst"),
            "data/aggregates/conversations.jsonl.zst",
            &convs,
            &run,
        )?;
        write_stream(
            &agg.join("sessions.jsonl.zst"),
            "data/aggregates/sessions.jsonl.zst",
            &sessions,
            &run,
        )?;
        write_stream(
            &agg.join("projects.jsonl.zst"),
            "data/aggregates/projects.jsonl.zst",
            &projects,
            &run,
        )?;
        write_stream(
            &agg.join("machines.jsonl.zst"),
            "data/aggregates/machines.jsonl.zst",
            &machines,
            &run,
        )?;
        write_stream(
            &agg.join("artifacts.jsonl.zst"),
            "data/aggregates/artifacts.jsonl.zst",
            &artifacts,
            &run,
        )?;
        let _ = summary;
        Ok(())
    }

    fn write_manifest(&mut self, summary: &RunStats, d: &DiscoverStats) -> Result<()> {
        let mut m = Manifest::load(&self.layout.root).unwrap_or_else(|| {
            Manifest::new(
                env!("CARGO_PKG_VERSION"),
                self.opts.secrets_str(),
                &self.started_at,
            )
        });
        m.format = crate::dataset::DATASET_FORMAT.into(); // upgrades a legacy id on append
        m.updated_at = timeutil::now_utc().to_rfc3339();
        m.tool_version = env!("CARGO_PKG_VERSION").to_string();
        m.redaction_policy = self.opts.secrets_str().to_string();

        let st = self.state.borrow();
        // Counts describe the dataset, not just this run.
        m.counts.sources = st.ledger.count("select count(*) from source")?;
        m.counts.conversations = st.ledger.count("select count(*) from conversation")?;
        m.counts.sessions = st.ledger.count("select count(*) from session")?;
        m.counts.events = st.ledger.count("select count(*) from event_index")?;
        m.counts.tool_calls = st.ledger.count(
            "select count(*) from event_index where event_type in ('tool_call','tool_result')",
        )?;
        m.counts.artifacts = st.ledger.count("select count(*) from artifact_ref")?;
        m.counts.projects = st.ledger.count("select count(*) from project")?;
        m.counts.machines = st.ledger.count("select count(*) from machine")?;
        m.counts.conflicts = st.ledger.count("select count(*) from conflict")?;
        m.counts.redacted_events = st.redacted_events;
        m.counts.date_range = st.ledger.date_range()?.map(|(a, b)| [a, b]);
        let shards = st.shards.clone();
        drop(st);

        let mut event_shards: Vec<ShardRef> = m
            .event_shards
            .iter()
            .filter(|s| !shards.iter().any(|n| n.path == s.path))
            .cloned()
            .collect();
        event_shards.extend(shards);
        event_shards.sort_by(|a, b| a.path.cmp(&b.path));
        m.event_shards = event_shards;
        m.import_runs.push(ImportRunRecord {
            import_run: self.run.clone(),
            started_at: self.started_at.clone(),
            finished_at: summary.finished_at.clone(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            schema_version: crate::model::SCHEMA_VERSION,
            secret_policy: self.opts.secrets_str().into(),
            args: self.opts.args.clone(),
            events_new: summary.events_new,
            events_duplicate: summary.events_duplicate,
            sources_examined: summary.candidate_sources,
            sources_skipped: summary.sources_skipped,
            sources_failed: summary.sources_failed,
            parse_errors: summary.parse_errors,
            state: if self.opts.dry_run {
                "dry_run".into()
            } else {
                "complete".into()
            },
            notes: d.errors.iter().take(50).cloned().collect(),
        });
        dataset::write_atomic(
            &self.layout.manifest(),
            serde_json::to_string_pretty(&m)?.as_bytes(),
        )
    }

    fn write_inventory(&mut self, d: &DiscoverStats, summary: &RunStats) -> Result<()> {
        let st = self.state.borrow();
        let mut sources = st
            .inventory
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        sources.extend(d.failed_sources.iter().cloned());
        let inv = json!({
            "tool": "convolith",
            "tool_version": env!("CARGO_PKG_VERSION"),
            "import_run": self.run,
            "generated_at": timeutil::now_utc().to_rfc3339(),
            "files_seen": d.files_seen,
            "files_skipped": d.files_skipped,
            "bytes_seen": d.bytes_seen,
            "archives_expanded": d.archives_expanded,
            "archive_entries": d.archive_entries,
            "archive_entries_rejected": d.archive_entries_rejected,
            "sources": sources,
            "counts": {
                "candidate_sources": summary.candidate_sources,
                "supported_sources": summary.supported_sources,
                "unsupported_sources": summary.unsupported_sources,
                "sources_failed": summary.sources_failed,
            },
            "discovery_notes": d.errors.iter().take(500).collect::<Vec<_>>(),
        });
        drop(st);
        if self.opts.dry_run {
            return Ok(());
        }
        dataset::write_atomic(
            &self.layout.reports().join(INVENTORY_JSON),
            serde_json::to_string_pretty(&inv)?.as_bytes(),
        )
    }
}

fn write_stream<T: serde::Serialize>(path: &Path, rel: &str, items: &[T], run: &str) -> Result<()> {
    let mut w = dataset::JsonlZstWriter::create(path, 9)?;
    for i in items {
        w.write_record(i)?;
    }
    w.finish(rel, run)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-source processing
// ---------------------------------------------------------------------------

fn detection_of(registry: &Registry, probe: &Probe, only: Option<&str>) -> Option<Detection> {
    let hits = registry.detect_all(probe);
    hits.into_iter()
        .find(|d| only.is_none() || only == Some(d.parser.as_str()))
}

/// Classify one source, parse it when a parser claims it, and account for the
/// outcome honestly.
#[allow(clippy::too_many_arguments)]
fn import_source(
    registry: &Registry,
    state: &Rc<RefCell<ImportState>>,
    staging: &Path,
    source: &Source,
    probe: &Probe,
    opts: &ImportOptions,
    secrets: &Secrets,
    cfg: &Config,
    run: String,
) -> Result<()> {
    let detection = detection_of(registry, probe, opts.only_parser.as_deref());
    let base = InventoryRow {
        source_id: source.source_id.clone(),
        original_path: source.display_path.clone(),
        container_chain: source.container_chain.clone(),
        size: source.size,
        mtime: source.mtime.map(|m| m.to_rfc3339()),
        sha256: None,
        machine_id: source.machine_id.clone(),
        platform: source.platform.clone(),
        format: detection
            .as_ref()
            .map(|d| d.format.clone())
            .unwrap_or_else(|| "unknown".into()),
        provider_guess: detection.as_ref().map(|d| d.provider.clone()),
        application_guess: detection.as_ref().map(|d| d.application.clone()),
        parser: detection
            .as_ref()
            .map(|d| d.parser.clone())
            .unwrap_or_default(),
        detection_confidence: detection
            .as_ref()
            .map(|d| d.confidence.as_str().to_string())
            .unwrap_or_else(|| "none".into()),
        status: "discovered".into(),
        records_examined: 0,
        records_imported: 0,
        records_duplicate: 0,
        records_skipped: 0,
        records_failed: 0,
        note: None,
    };

    let Some(detection) = detection else {
        // Nothing claims this file. It is inventoried, not guessed at.
        let mut st = state.borrow_mut();
        st.counts.unsupported_sources += 1;
        let mut row = base;
        row.status = "unsupported".into();
        row.note = Some("no parser claimed this file; left unresolved".into());
        if let Some((format, reason)) = crate::parsers::known_unsupported(probe) {
            row.format = format.into();
            row.note = Some(format!("{reason}; left unresolved"));
        }
        let run = st.run.clone();
        st.ledger.record_source(&row.to_source_record(&run))?;
        st.inventory.push(row);
        return Ok(());
    };

    let Some(parser) = registry.by_id(&detection.parser) else {
        anyhow::bail!("registry has no parser {:?}", detection.parser);
    };

    // Resume: skip a source whose bytes and parser version are unchanged.
    let fingerprint = dedup::file_fingerprint(&source.read_path);
    let state_key = format!("{}|{}", source.source_id, detection.parser);
    if opts.resume && !fingerprint.is_empty() {
        let st = state.borrow();
        if st
            .ledger
            .is_source_complete(&state_key, &fingerprint, parser.id(), parser.version())
        {
            drop(st);
            let mut st = state.borrow_mut();
            st.counts.sources_skipped += 1;
            let mut row = base;
            row.status = "skipped_unchanged".into();
            row.note =
                Some("resume: bytes and parser version unchanged since a previous run".into());
            st.inventory.push(row);
            // Keep the source visible to a later run.
            st.ledger.record_source(&SourceRecord {
                source_id: source.source_id.clone(),
                display_path: source.display_path.clone(),
                original_path: source.full_path_for_record(),
                container_chain: source.container_chain.clone(),
                size: source.size,
                mtime: source.mtime.map(|m| m.to_rfc3339()),
                sha256: None,
                machine_id: source.machine_id.clone(),
                platform: source.platform.clone(),
                provider_guess: Some(detection.provider.clone()),
                application_guess: Some(detection.application.clone()),
                format: Some(detection.format.clone()),
                parser: Some(parser.id().to_string()),
                parser_version: Some(parser.version().to_string()),
                detection_confidence: Some(detection.confidence.as_str().to_string()),
                status: "skipped_unchanged".into(),
                import_run: run.clone(),
                first_seen: timeutil::now_utc().to_rfc3339(),
                last_seen: timeutil::now_utc().to_rfc3339(),
                records_examined: 0,
                records_imported: 0,
                records_duplicate: 0,
                records_skipped: 0,
                records_failed: 0,
                notes: "resume: unchanged".into(),
            })?;
            return Ok(());
        }
    }

    // Hash the bytes we are about to read, for provenance. Large files are
    // hashed from the original, read-only handle.
    let sha256 = crate::id::sha256_file(&source.read_path).ok();

    // Labels come from detection, so the sink and the inventory agree without
    // the parser having to repeat itself.
    let mut labelled = source.clone();
    labelled.provider_label = Some(detection.provider.clone());
    labelled.application_label = Some(detection.application.clone());
    labelled.parser_id = Some(parser.id().to_string());
    if labelled.machine_id.is_none() && parser.is_web_export() {
        labelled.machine_id = crate::collect::importing_machine_id();
    }
    let source = &labelled;

    let mut ctx = ImportCtx {
        state: state.clone(),
        staging: staging.to_path_buf(),
        machine_id: source.machine_id.clone(),
        platform: source.platform.clone(),
        run: run.clone(),
        secrets: secrets.clone_for_thread(),
        max_record_bytes: opts.max_record_bytes,
        max_file_bytes: opts.max_file_bytes,
        max_inline_text_bytes: opts.max_inline_text_bytes,
        parser_provider: parser.provider().to_string(),
        parser_application: parser.application().to_string(),
        parser_id: parser.id().to_string(),
        parser_version: parser.version().to_string(),
        notes: Vec::new(),
    };
    let mut sink = ImportSink {
        state: state.clone(),
        run: run.clone(),
        source: source.clone(),
        sha256: sha256.clone(),
        meta: ConversationMeta::default(),
        conversation_id: String::new(),
        conversation_machine_id: source.machine_id.clone(),
        tier: IdentityHint::Fingerprint,
        seq: 0,
        secrets: secrets.clone_for_thread(),
        max_inline_text_bytes: opts.max_inline_text_bytes,
        provider: parser.provider().to_string(),
        application: parser.application().to_string(),
        parser_id: parser.id().to_string(),
        parser_version: parser.version().to_string(),
        session_started: None,
        session_ended: None,
        events_emitted: 0,
        open: false,
        new_events: 0,
        duplicates: 0,
        cfg: cfg.clone(),
        project: None,
    };

    let outcome = parser.parse(&mut ctx, source, &mut sink);
    let ctx_notes = std::mem::take(&mut ctx.notes);
    let new_events = sink.new_events;
    let duplicates = sink.duplicates;
    let report = match outcome {
        Ok(r) => r,
        Err(e) => {
            // A damaged source is reported; other sources keep going.
            let mut msg = crate::secrets::scrub_for_log(&format!("{e:#}"));
            // Events emitted before the error are already persisted: write the
            // conversation/session rows they reference, and account for them.
            if sink.open {
                if let Err(e2) = sink.end() {
                    msg.push_str(&format!(
                        "; finalizing the partial conversation failed: {}",
                        crate::secrets::scrub_for_log(&format!("{e2:#}"))
                    ));
                }
            }
            let examined = new_events + duplicates + 1;
            let mut st = state.borrow_mut();
            st.counts.sources_failed += 1;
            st.counts.records_failed += 1;
            st.counts.records_examined += examined;
            st.counts.records_imported += new_events;
            st.counts.records_duplicate += duplicates;
            st.counts.parse_errors += 1;
            st.quality.push(format!("{}: {msg}", source.display_path));
            let mut row = base;
            row.status = "failed".into();
            row.sha256 = sha256.clone();
            row.records_examined = examined;
            row.records_imported = new_events;
            row.records_duplicate = duplicates;
            row.records_failed = 1;
            row.note = Some(msg.clone());
            st.inventory.push(row);
            st.ledger.record_source(&SourceRecord {
                source_id: source.source_id.clone(),
                display_path: source.display_path.clone(),
                original_path: source.full_path_for_record(),
                container_chain: source.container_chain.clone(),
                size: source.size,
                mtime: source.mtime.map(|m| m.to_rfc3339()),
                sha256,
                machine_id: source.machine_id.clone(),
                platform: source.platform.clone(),
                provider_guess: Some(detection.provider.clone()),
                application_guess: Some(detection.application.clone()),
                format: Some(detection.format.clone()),
                parser: Some(parser.id().to_string()),
                parser_version: Some(parser.version().to_string()),
                detection_confidence: Some(detection.confidence.as_str().to_string()),
                status: "failed".into(),
                import_run: run.clone(),
                first_seen: timeutil::now_utc().to_rfc3339(),
                last_seen: timeutil::now_utc().to_rfc3339(),
                records_examined: examined,
                records_imported: new_events,
                records_duplicate: duplicates,
                records_skipped: 0,
                records_failed: 1,
                notes: msg.clone(),
            })?;
            st.ledger.record_parse_error(
                &source.source_id,
                Some(&source.display_path),
                None,
                &msg,
                &run,
                &timeutil::now_utc().to_rfc3339(),
            )?;
            return Ok(());
        }
    };

    // Every emitted event is exactly one of new/duplicate; `events_emitted`
    // restarts per conversation, so it cannot be used for a whole source.
    let events = new_events + duplicates;
    let mut st = state.borrow_mut();
    st.counts.supported_sources += 1;
    // Record accounting comes from what actually happened, not from the
    // parser's own report, so the identity in `validate` cannot be fudged.
    st.counts.records_examined += events + report.records_skipped + report.records_failed;
    st.counts.records_imported += new_events;
    st.counts.records_duplicate += duplicates;
    st.counts.records_skipped += report.records_skipped;
    st.counts.records_failed += report.records_failed;
    st.counts.parse_errors += report.records_failed;
    let mut row = base;
    row.status = if report.records_failed > 0 {
        "partially_parsed".into()
    } else {
        "parsed".into()
    };
    row.sha256 = sha256.clone();
    row.records_examined = events + report.records_skipped + report.records_failed;
    row.records_imported = new_events;
    row.records_duplicate = duplicates;
    row.records_skipped = report.records_skipped;
    row.records_failed = report.records_failed;
    let safe_notes: Vec<String> = report
        .notes
        .iter()
        .map(|note| crate::secrets::scrub_for_log(note))
        .collect();
    row.note = if safe_notes.is_empty() {
        None
    } else {
        Some(safe_notes.join("; "))
    };
    if !ctx_notes.is_empty() {
        st.quality.extend(ctx_notes.iter().cloned());
    }
    st.inventory.push(row);
    st.ledger.record_source(&SourceRecord {
        source_id: source.source_id.clone(),
        display_path: source.display_path.clone(),
        original_path: source.full_path_for_record(),
        container_chain: source.container_chain.clone(),
        size: source.size,
        mtime: source.mtime.map(|m| m.to_rfc3339()),
        sha256: sha256.clone(),
        machine_id: source.machine_id.clone(),
        platform: source.platform.clone(),
        provider_guess: Some(detection.provider.clone()),
        application_guess: Some(detection.application.clone()),
        format: Some(detection.format.clone()),
        parser: Some(parser.id().to_string()),
        parser_version: Some(parser.version().to_string()),
        detection_confidence: Some(detection.confidence.as_str().to_string()),
        status: if report.records_failed > 0 {
            "partially_parsed".into()
        } else {
            "parsed".into()
        },
        import_run: run.clone(),
        first_seen: timeutil::now_utc().to_rfc3339(),
        last_seen: timeutil::now_utc().to_rfc3339(),
        records_examined: events + report.records_skipped + report.records_failed,
        records_imported: new_events,
        records_duplicate: duplicates,
        records_skipped: report.records_skipped,
        records_failed: report.records_failed,
        notes: safe_notes.join("; "),
    })?;
    if report.records_failed > 0 {
        st.ledger.record_parse_error(
            &source.source_id,
            Some(&source.display_path),
            None,
            &format!("{} record(s) failed to parse", report.records_failed),
            &run,
            &timeutil::now_utc().to_rfc3339(),
        )?;
    }
    // Only a source that parsed without failure is a resume candidate.
    if !opts.dry_run && !fingerprint.is_empty() {
        st.ledger.mark_source_complete(
            &state_key,
            &source.source_id,
            &fingerprint,
            parser.id(),
            parser.version(),
            &run,
            &timeutil::now_utc().to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parse context and event sink
// ---------------------------------------------------------------------------

pub struct ImportCtx {
    state: Rc<RefCell<ImportState>>,
    staging: PathBuf,
    machine_id: Option<String>,
    platform: Option<String>,
    run: String,
    secrets: Secrets,
    max_record_bytes: usize,
    max_file_bytes: u64,
    max_inline_text_bytes: usize,
    parser_provider: String,
    parser_application: String,
    parser_id: String,
    parser_version: String,
    notes: Vec<String>,
}

impl ParseContext for ImportCtx {
    fn staging_dir(&self) -> &Path {
        &self.staging
    }
    fn machine_id(&self) -> Option<&str> {
        self.machine_id.as_deref()
    }
    fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }
    fn import_run(&self) -> &str {
        &self.run
    }
    fn provider(&self) -> &str {
        &self.parser_provider
    }
    fn application(&self) -> &str {
        &self.parser_application
    }
    fn parser_id(&self) -> &str {
        &self.parser_id
    }
    fn parser_version(&self) -> &str {
        &self.parser_version
    }
    fn store_artifact(
        &mut self,
        bytes: &[u8],
        filename: Option<&str>,
        mime: Option<&str>,
        source_path: Option<&str>,
    ) -> Result<String> {
        let mut st = self.state.borrow_mut();
        let (sha, rel, size, copied) = st.artifacts.put_bytes(bytes, filename)?;
        let artifact_id = crate::id::id("art_", &["sha256", &sha]);
        st.artifacts_out.push(ArtifactRecord {
            schema_version: crate::model::SCHEMA_VERSION,
            artifact_id: artifact_id.clone(),
            sha256: sha,
            size,
            mime: mime
                .map(str::to_string)
                .or_else(|| filename.and_then(guess_mime)),
            filename: filename.map(str::to_string),
            source_path: source_path.map(str::to_string),
            event_ids: Vec::new(),
            stored_path: rel,
            copied,
        });
        Ok(artifact_id)
    }
    fn apply_secret_policy(&self, field: &str, text: &str) -> (String, Option<RedactionHit>) {
        self.secrets.apply(field, text)
    }
    fn max_record_bytes(&self) -> usize {
        self.max_record_bytes
    }
    fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }
    fn max_inline_text_bytes(&self) -> usize {
        self.max_inline_text_bytes
    }
    fn note(&mut self, message: String) {
        self.notes.push(crate::secrets::scrub_for_log(&message));
    }
}

pub struct ImportSink {
    state: Rc<RefCell<ImportState>>,
    run: String,
    source: Source,
    sha256: Option<String>,
    meta: ConversationMeta,
    conversation_id: String,
    conversation_machine_id: Option<String>,
    tier: IdentityHint,
    seq: u64,
    secrets: Secrets,
    max_inline_text_bytes: usize,
    provider: String,
    application: String,
    parser_id: String,
    parser_version: String,
    session_started: Option<String>,
    session_ended: Option<String>,
    events_emitted: u64,
    /// A conversation has begun and not yet ended.
    open: bool,
    /// New canonical records written because of this source.
    new_events: u64,
    /// Records that matched an existing canonical event with identical content.
    duplicates: u64,
    cfg: Config,
    project: Option<crate::identity::ResolvedProject>,
}

impl ImportSink {
    /// `.git/config` of a working directory that exists on *this* machine. Only
    /// trusted for a session file read from the current user's home (a live
    /// local store): a cwd recorded on another machine, or inside a backup,
    /// may name a path that exists here and belongs to a different repository.
    /// A repository rooted at or above the home directory (dotfiles) is not
    /// evidence about a project below it.
    fn local_repo_remote(&self) -> Option<String> {
        if !self.source.container_chain.is_empty() {
            return None;
        }
        let home = std::path::PathBuf::from(
            std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?,
        );
        let home = home.canonicalize().unwrap_or(home);
        if !self.source.read_path.starts_with(&home) {
            return None;
        }
        let wd = std::path::Path::new(self.meta.working_directory.as_deref()?);
        if !wd.is_absolute() {
            return None;
        }
        let (root, remote) = crate::identity::find_repo(wd, 8)?;
        let root = root.canonicalize().unwrap_or(root);
        if home.starts_with(&root) {
            return None;
        }
        remote
    }

    /// Repository evidence is read as data, never executed, and is resolved
    /// once per conversation: it is the same for every event in it. Called at
    /// `begin` so events carry project/repository ids, and again at `end`.
    fn resolve_project(&mut self) {
        let remote: Option<String> = self
            .meta
            .git_remote
            .clone()
            .or_else(|| {
                self.meta.repository_root.as_deref().and_then(|root| {
                    crate::identity::read_git_config(
                        &std::path::Path::new(root).join(".git").join("config"),
                    )
                    .and_then(|t| crate::identity::git_remote(&t))
                })
            })
            .or_else(|| self.local_repo_remote());
        self.project = crate::identity::resolve_project(
            &self.cfg,
            self.meta.working_directory.as_deref(),
            remote.as_deref(),
            self.meta
                .repository_root
                .as_deref()
                .map(std::path::Path::new),
        );
    }
}

impl EventSink for ImportSink {
    fn begin(&mut self, meta: ConversationMeta) -> Result<()> {
        let fallback_owned: Vec<String> = match &meta.native_id {
            Some(_) => Vec::new(),
            None => vec![
                meta.title.clone().unwrap_or_default(),
                meta.working_directory.clone().unwrap_or_default(),
                self.source
                    .inner_path
                    .clone()
                    .unwrap_or_else(|| self.source.display_path.clone()),
            ],
        };
        let fallback: Vec<&str> = fallback_owned.iter().map(String::as_str).collect();
        let (cid, tier) = dedup::conversation_key(
            &self.provider,
            &self.application,
            meta.native_id.as_deref(),
            meta.native_session_id.as_deref(),
            &fallback,
        );
        self.conversation_id = cid.clone();
        self.conversation_machine_id = {
            let state = self.state.borrow();
            state
                .ledger
                .conversation_machine_id(&cid)?
                .or_else(|| self.source.machine_id.clone())
        };
        // `Native` is the parser asserting its records carry globally unique
        // provider ids. Anything else falls back to the conservative
        // content+position fingerprint, which can only merge records that agree
        // on both.
        let _ = tier;
        self.tier = if meta.identity_hint == IdentityHint::Native {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        };
        self.seq = 0;
        self.open = true;
        self.events_emitted = 0;
        self.session_started = meta.started_at.as_ref().and_then(|s| s.rfc3339());
        self.session_ended = meta.ended_at.as_ref().and_then(|s| s.rfc3339());
        self.meta = meta;
        self.resolve_project();
        Ok(())
    }

    fn emit(&mut self, draft: EventDraft) -> Result<()> {
        let seq = self.seq;
        self.seq += 1;
        self.events_emitted += 1;

        let art_start = self.state.borrow().artifacts_out.len();
        let (event, native_id) = self.build_event(draft, seq)?;
        let content_fp = dedup::content_fingerprint(&event);
        let input = IdentityInput {
            provider: &self.provider,
            application: &self.application,
            conversation_key: &self.conversation_id,
            native_id: native_id.as_deref(),
            seq,
            role: event.role.as_str(),
            event_type: event_type_str(event.event_type),
            hint: self.tier,
        };
        let ident = dedup::event_identity(&input, &content_fp);
        let mut event = event;
        event.event_id = ident.event_id.clone();

        let mut st = self.state.borrow_mut();
        for rec in st.artifacts_out.iter_mut().skip(art_start) {
            rec.event_ids.push(ident.event_id.clone());
        }
        if st.writer.is_none() && !st.dry_run {
            // The first event of the run opens the shard.
            drop(st);
            self.open_shard_for_state()?;
            st = self.state.borrow_mut();
        }
        let shard_rel = st.shard_rel.clone();
        let now = timeutil::now_utc().to_rfc3339();

        match st.ledger.check_event(&ident.event_id, &content_fp)? {
            EventStatus::New => {
                let p = self.provenance_ref(seq, native_id.as_deref(), &self.source, &shard_rel);
                st.ledger.insert_event(
                    &ident.event_id,
                    &event.conversation_id,
                    event.session_id.as_deref(),
                    seq,
                    event.role.as_str(),
                    event_type_str(event.event_type),
                    &event.provider,
                    &event.application,
                    event.project_id.as_deref(),
                    event.repository_id.as_deref(),
                    event.machine_id.as_deref(),
                    event.timestamp.as_deref(),
                    &content_fp,
                    ident.tier,
                    &shard_rel,
                    &now,
                    None,
                )?;
                st.ledger
                    .add_observation(&ident.event_id, &p, &self.source.source_id)?;
                st.write_event(&event)?;
                st.counts.events_new += 1;
                st.counts.events_total += 1;
                self.new_events += 1;
            }
            EventStatus::Duplicate => {
                let p = self.provenance_ref(seq, native_id.as_deref(), &self.source, &shard_rel);
                let added =
                    st.ledger
                        .add_observation(&ident.event_id, &p, &self.source.source_id)?;
                let _ = added;
                st.counts.events_duplicate += 1;
                st.counts.events_total += 1;
                self.duplicates += 1;
            }
            EventStatus::Conflict {
                content_fp: existing,
            } => {
                // Never silently pick a winner: the variant is kept as its own
                // event, linked to the original, and reported in CONFLICTS.md.
                let variant_id = crate::id::id("ev_", &["variant", &ident.event_id, &content_fp]);
                let is_new = st.ledger.check_event(&variant_id, &content_fp)? == EventStatus::New;
                let p = self.provenance_ref(seq, native_id.as_deref(), &self.source, &shard_rel);
                if is_new {
                    let mut ve = event.clone();
                    ve.event_id = variant_id.clone();
                    ve.metadata
                        .insert("variant_of".into(), json!(ident.event_id));
                    ve.metadata
                        .insert("conflict_existing_content_fp".into(), json!(existing));
                    st.ledger.insert_event(
                        &variant_id,
                        &ve.conversation_id,
                        ve.session_id.as_deref(),
                        seq,
                        ve.role.as_str(),
                        event_type_str(ve.event_type),
                        &ve.provider,
                        &ve.application,
                        ve.project_id.as_deref(),
                        ve.repository_id.as_deref(),
                        ve.machine_id.as_deref(),
                        ve.timestamp.as_deref(),
                        &content_fp,
                        ident.tier,
                        &shard_rel,
                        &now,
                        Some(&ident.event_id),
                    )?;
                    st.ledger
                        .add_observation(&variant_id, &p, &self.source.source_id)?;
                    st.write_event(&ve)?;
                    st.counts.events_new += 1;
                    self.new_events += 1;
                } else {
                    st.ledger
                        .add_observation(&variant_id, &p, &self.source.source_id)?;
                    st.counts.events_duplicate += 1;
                    self.duplicates += 1;
                }
                let recorded = st.ledger.record_conflict(
                    &ident.event_id,
                    &variant_id,
                    "same_identity_different_content",
                    "the same canonical identity was observed with different content; both variants are preserved",
                    &content_fp,
                    &self.source.source_id,
                    Some(&self.source.display_path),
                    &now,
                )?;
                st.counts.events_total += 1;
                if recorded {
                    st.counts.conflicts += 1;
                    st.quality.push(format!(
                        "conflict: {} has a differing variant observed in {}",
                        ident.event_id, self.source.display_path
                    ));
                }
            }
        }

        st.ledger.bump_conversation(&event.conversation_id, seq)?;
        if event.event_type == EventType::ToolCall {
            st.counts.tool_calls += 1;
        }
        if !event.redactions.is_empty() {
            st.redacted_events += 1;
            let kinds: Vec<(String, u32)> = event
                .redactions
                .iter()
                .map(|r| (r.kind.clone(), r.count))
                .collect();
            for (kind, count) in kinds {
                *st.secret_kinds.entry(kind).or_insert(0) += count as u64;
            }
        }
        Ok(())
    }

    fn end(&mut self) -> Result<()> {
        self.open = false;
        self.resolve_project();

        let mut st = self.state.borrow_mut();
        let conv = crate::model::Conversation {
            schema_version: crate::model::SCHEMA_VERSION,
            conversation_id: self.conversation_id.clone(),
            session_id: self.meta.native_session_id.clone(),
            provider: self.provider.clone(),
            application: self.application.clone(),
            title: self.meta.title.clone(),
            model: self.meta.model.clone(),
            agent: self.meta.agent.clone(),
            machine_id: self.conversation_machine_id.clone(),
            project_id: self.project.as_ref().map(|p| p.project_id.clone()),
            repository_id: self.project.as_ref().and_then(|p| p.repository_id.clone()),
            working_directory: self.meta.working_directory.clone(),
            started_at: self.session_started.clone(),
            ended_at: self.session_ended.clone(),
            event_count: self.events_emitted,
            source_ids: vec![self.source.source_id.clone()],
            metadata: Map::new(),
        };
        st.ledger.upsert_conversation(&conv, tier_str(self.tier))?;
        if let Some(p) = &self.project {
            st.ledger.upsert_project(
                &Project {
                    project_id: p.project_id.clone(),
                    repository_id: p.repository_id.clone(),
                    name: p.name.clone(),
                    paths: self.meta.working_directory.clone().into_iter().collect(),
                    machines: self.source.machine_id.clone().into_iter().collect(),
                    branches: self.meta.branch.clone().into_iter().collect(),
                    evidence: p.evidence.clone(),
                    event_count: self.events_emitted,
                },
                self.meta.working_directory.as_deref(),
                self.source.machine_id.as_deref(),
            )?;
        }
        if let Some(m) = &self.source.machine_id {
            st.ledger.upsert_machine(&Machine {
                machine_id: m.clone(),
                platform: self.source.platform.clone(),
                evidence: vec![self.source.display_path.clone()],
                source_ids: vec![self.source.source_id.clone()],
                event_count: self.events_emitted,
            })?;
        }
        for rec in std::mem::take(&mut st.artifacts_out) {
            let ids: Vec<&str> = if rec.event_ids.is_empty() {
                vec![""]
            } else {
                rec.event_ids.iter().map(String::as_str).collect()
            };
            for event_id in ids {
                st.ledger.upsert_artifact(
                    &rec.artifact_id,
                    &rec.sha256,
                    rec.size,
                    rec.mime.as_deref(),
                    rec.filename.as_deref(),
                    rec.source_path.as_deref(),
                    &rec.stored_path,
                    rec.copied,
                    event_id,
                )?;
            }
        }
        if let Some(sid) = &self.meta.native_session_id {
            st.ledger.upsert_session(&Session {
                schema_version: crate::model::SCHEMA_VERSION,
                session_id: dedup::session_id_for(&self.provider, &self.application, sid),
                conversation_id: self.conversation_id.clone(),
                provider: self.provider.clone(),
                application: self.application.clone(),
                title: self.meta.title.clone(),
                agent: self.meta.agent.clone(),
                machine_id: self.conversation_machine_id.clone(),
                project_id: self.project.as_ref().map(|p| p.project_id.clone()),
                working_directory: self.meta.working_directory.clone(),
                started_at: self.session_started.clone(),
                ended_at: self.session_ended.clone(),
                event_count: self.events_emitted,
                source_ids: vec![self.source.source_id.clone()],
            })?;
            st.counts.sessions += 1;
        }
        st.counts.conversations += 1;
        Ok(())
    }
}

impl ImportSink {
    fn open_shard_for_state(&self) -> Result<()> {
        let mut st = self.state.borrow_mut();
        if st.writer.is_some() || st.dry_run {
            return Ok(());
        }
        let n = st.layout.next_shard_index()?;
        st.shard_rel = format!("data/events/part-{n:06}.jsonl.zst");
        let full = st.layout.root.join(&st.shard_rel);
        std::fs::create_dir_all(st.layout.events_dir())?;
        st.writer = Some(dataset::JsonlZstWriter::create(&full, 9)?);
        Ok(())
    }

    fn provenance_ref(
        &self,
        seq: u64,
        native_id: Option<&str>,
        source: &Source,
        _shard: &str,
    ) -> ProvenanceRef {
        ProvenanceRef {
            source_id: source.source_id.clone(),
            source_path: source.display_path.clone(),
            container_chain: source.container_chain.clone(),
            record_index: Some(seq),
            record_id: native_id.map(str::to_string),
            parser: self.parser_id.clone(),
            parser_version: self.parser_version.clone(),
            source_sha256: self.sha256.clone(),
            first_seen: timeutil::now_utc().to_rfc3339(),
            import_run: self.run.clone(),
            identity_tier: tier_str(self.tier).to_string(),
        }
    }

    fn build_event(&mut self, draft: EventDraft, seq: u64) -> Result<(Event, Option<String>)> {
        let mut redactions = Vec::new();
        let mut content = Vec::with_capacity(draft.content.len());
        for (i, part) in draft.content.into_iter().enumerate() {
            content.push(self.prepare_part(part, i, &mut redactions)?);
        }
        let ts = draft.timestamp;
        if let Some(r) = ts.rfc3339() {
            if self.session_started.is_none() {
                self.session_started = Some(r.clone());
            }
            self.session_ended = Some(r);
        }
        let native_id = draft.native_id.clone();
        let mut metadata = draft.metadata;
        if let Some(n) = native_id.clone() {
            metadata.insert("native_id".into(), json!(n));
        }
        if let Some(n) = draft.parent_native_id {
            metadata.insert("parent_native_id".into(), json!(n));
        }
        if !draft.tool_call_ids.is_empty() {
            metadata.insert("tool_call_ids".into(), json!(draft.tool_call_ids));
        }
        let event = Event {
            schema_version: crate::model::SCHEMA_VERSION,
            event_id: String::new(),
            conversation_id: self.conversation_id.clone(),
            session_id: self.meta.native_session_id.clone(),
            parent_event_id: None,
            seq,
            timestamp: ts.rfc3339(),
            timestamp_original: ts.original.clone(),
            timestamp_confidence: ts.confidence,
            role: draft.role,
            event_type: draft.event_type,
            provider: self.provider.clone(),
            application: self.application.clone(),
            model: draft.model.or_else(|| self.meta.model.clone()),
            agent: draft.agent.or_else(|| self.meta.agent.clone()),
            machine_id: self.conversation_machine_id.clone(),
            project_id: self.project.as_ref().map(|p| p.project_id.clone()),
            repository_id: self.project.as_ref().and_then(|p| p.repository_id.clone()),
            working_directory: self.meta.working_directory.clone(),
            branch: self.meta.branch.clone(),
            commit: None,
            worktree_id: None,
            content,
            metadata,
            redactions,
            provenance: Vec::new(),
        };
        Ok((event, native_id))
    }

    fn prepare_part(
        &mut self,
        part: Part,
        index: usize,
        redactions: &mut Vec<Redaction>,
    ) -> Result<Part> {
        let field = format!("content[{index}]");
        Ok(match part {
            Part::Text { text, truncation } => {
                let (text, hits) = self.secrets.apply_detailed(&field, &text);
                for h in hits {
                    push_hit(redactions, h);
                }
                let (text, truncation) = self.maybe_truncate(text, truncation)?;
                Part::Text { text, truncation }
            }
            Part::Reasoning { text, visibility } => {
                let (text, hits) = self.secrets.apply_detailed(&field, &text);
                for h in hits {
                    push_hit(redactions, h);
                }
                let (text, _) = self.maybe_truncate(text, None)?;
                Part::Reasoning { text, visibility }
            }
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                // Tool arguments routinely carry credentials, so they pass
                // through the same policy as prose.
                let (arguments, hits) = self.secrets.apply_json_detailed(&field, &arguments);
                for h in hits {
                    push_hit(redactions, h);
                }
                Part::ToolCall {
                    id,
                    name,
                    arguments,
                }
            }
            Part::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => {
                let (output, hits) = self.secrets.apply_json_detailed(&field, &output);
                for h in hits {
                    push_hit(redactions, h);
                }
                let output = self.maybe_spill_output(output)?;
                Part::ToolResult {
                    tool_call_id,
                    output,
                    is_error,
                }
            }
            Part::Opaque { kind, note, raw } => {
                let (raw, hits) = match raw {
                    Some(v) => {
                        let (nv, h) = self.secrets.apply_json_detailed(&field, &v);
                        (Some(nv), h)
                    }
                    None => (None, Vec::new()),
                };
                for h in hits {
                    push_hit(redactions, h);
                }
                Part::Opaque { kind, note, raw }
            }
            other => other,
        })
    }

    /// Long text is kept in full as an artifact and referenced from the event,
    /// never truncated away: the canonical record stays small and nothing is
    /// lost (spec §32).
    fn maybe_truncate(
        &self,
        text: String,
        existing: Option<Truncation>,
    ) -> Result<(String, Option<Truncation>)> {
        if let Some(t) = existing {
            return Ok((text, Some(t)));
        }
        let limit = self.max_inline_text_bytes;
        if limit == 0 || text.len() <= limit {
            return Ok((text, None));
        }
        let full_sha256 = crate::id::sha256_bytes(text.as_bytes());
        let mut inline = String::new();
        for ch in text.chars() {
            if inline.len() + ch.len_utf8() > limit {
                break;
            }
            inline.push(ch);
        }
        let inline_bytes = inline.len() as u64;
        let artifact_id = self.spill_text(&text);
        Ok((
            inline,
            Some(Truncation {
                full_sha256,
                full_bytes: text.len() as u64,
                inline_bytes,
                artifact: artifact_id?,
            }),
        ))
    }

    /// Store `text` as a content-addressed artifact; returns its id.
    fn spill_text(&self, text: &str) -> Result<String> {
        let mut st = self.state.borrow_mut();
        let (sha, rel, _size, copied) = st.artifacts.put_bytes(text.as_bytes(), None)?;
        let artifact_id = crate::id::id("art_", &["sha256", &sha]);
        st.artifacts_out.push(ArtifactRecord {
            schema_version: crate::model::SCHEMA_VERSION,
            artifact_id: artifact_id.clone(),
            sha256: sha,
            size: text.len() as u64,
            mime: Some("text/plain".into()),
            filename: None,
            source_path: Some(self.source.display_path.clone()),
            event_ids: Vec::new(),
            stored_path: rel,
            copied,
        });
        Ok(artifact_id)
    }

    /// A tool output past the inline threshold is spilled to the artifact store
    /// (after redaction) and replaced by a reference plus a bounded preview.
    fn maybe_spill_output(&self, output: serde_json::Value) -> Result<serde_json::Value> {
        let limit = self.max_inline_text_bytes;
        let text = match &output {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if limit == 0 || text.len() <= limit {
            return Ok(output);
        }
        let mut preview = String::new();
        for ch in text.chars() {
            if preview.len() + ch.len_utf8() > limit {
                break;
            }
            preview.push(ch);
        }
        let artifact_id = self.spill_text(&text)?;
        Ok(json!({
            "artifact_ref": {
                "artifact_id": artifact_id,
                "sha256": crate::id::sha256_bytes(text.as_bytes()),
                "bytes": text.len(),
            },
            "inline_preview": preview,
        }))
    }
}

fn push_hit(redactions: &mut Vec<Redaction>, hit: RedactionHit) {
    if let Some(existing) = redactions
        .iter_mut()
        .find(|r| r.kind == hit.kind && r.field == hit.field)
    {
        existing.count += hit.count;
    } else {
        redactions.push(Redaction {
            kind: hit.kind,
            count: hit.count,
            field: hit.field,
        });
    }
}

pub fn event_type_str(t: EventType) -> &'static str {
    match t {
        EventType::Message => "message",
        EventType::Reasoning => "reasoning",
        EventType::ToolCall => "tool_call",
        EventType::ToolResult => "tool_result",
        EventType::Compaction => "compaction",
        EventType::SystemNote => "system_note",
        EventType::Attachment => "attachment",
        EventType::AgentSpawn => "agent_spawn",
        EventType::AgentResult => "agent_result",
        EventType::Interruption => "interruption",
        EventType::Error => "error",
        EventType::Opaque => "opaque",
    }
}

fn tier_str(h: IdentityHint) -> &'static str {
    match h {
        IdentityHint::Native => "native",
        IdentityHint::Coordinates => "coordinates",
        IdentityHint::Fingerprint => "fingerprint",
    }
}

/// Write `reports/PARSE_ERRORS.md` and the JSON companions from the ledger.
pub fn write_reports(
    layout: &Layout,
    ledger: &Ledger,
    inv: &[InventoryRow],
    quality: &[String],
    secret_kinds: &BTreeMap<String, u64>,
    redacted_events: u64,
) -> Result<()> {
    let reports = layout.reports();
    std::fs::create_dir_all(&reports)?;
    let errs = ledger.parse_errors()?;
    let mut md = String::from("# Parse errors\n\nThe import never stops because one source is damaged; every failure is listed here.\n\n");
    if errs.is_empty() {
        md.push_str("No parse errors were recorded.\n");
    } else {
        md.push_str(&format!("{} error record(s).\n\n", errs.len()));
        md.push_str("| source | locator | message |\n|---|---|---|\n");
        for (src, loc, msg) in errs.iter().take(2000) {
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                one_line(src),
                loc.clone().unwrap_or_default(),
                one_line(msg)
            ));
        }
    }
    dataset::write_atomic(&reports.join("PARSE_ERRORS.md"), md.as_bytes())?;
    dataset::write_atomic(
        &reports.join("PARSE_ERRORS.json"),
        serde_json::to_string_pretty(&json!({
            "count": errs.len(),
            "errors": errs.iter().map(|(s, l, m)| json!({"source": s, "locator": l, "message": m})).collect::<Vec<_>>(),
        }))?
        .as_bytes(),
    )?;

    // Privacy audit: what kinds of secrets were found, never the values.
    let mut pa = String::from(
        "# Privacy audit\n\nOnly counts are reported; no secret value ever reaches a log, a report or the manifest.\n\n",
    );
    pa.push_str(&format!(
        "Secret policy in force for the canonical data: recorded in `manifest.json` (`redaction_policy`).\n\nEvents carrying at least one detected secret: **{}**.\n\n",
        redacted_events
    ));
    if secret_kinds.is_empty() {
        pa.push_str("No secret-like material was detected.\n");
    } else {
        pa.push_str("| kind | occurrences |\n|---|---:|\n");
        for (k, v) in secret_kinds {
            pa.push_str(&format!("| {k} | {v} |\n"));
        }
    }
    dataset::write_atomic(&reports.join("PRIVACY_AUDIT.md"), pa.as_bytes())?;

    // Data quality: honest list of what is approximate or missing.
    let mut q = String::from("# Data quality\n\n");
    q.push_str(&format!("Inventory rows: {}.\n\n", inv.len()));
    let mut by_status: BTreeMap<&str, u64> = BTreeMap::new();
    for r in inv {
        *by_status.entry(r.status.as_str()).or_insert(0) += 1;
    }
    q.push_str("| status | files |\n|---|---:|\n");
    for (k, v) in &by_status {
        q.push_str(&format!("| {k} | {v} |\n"));
    }
    if !quality.is_empty() {
        q.push_str(&format!(
            "\n## Observations\n\n{} distinct note(s); first 500 shown.\n\n",
            quality.len()
        ));
        for n in quality.iter().take(500) {
            q.push_str(&format!("- {}\n", one_line(n)));
        }
    }
    dataset::write_atomic(&reports.join("DATA_QUALITY.md"), q.as_bytes())?;
    Ok(())
}

pub fn one_line(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_names_are_stable() {
        assert_eq!(event_type_str(EventType::ToolCall), "tool_call");
        assert_eq!(event_type_str(EventType::AgentSpawn), "agent_spawn");
    }

    use crate::parser::SourceParser;
    use crate::source::{Capabilities, Confidence, ParseReport};

    /// Emits one event per line of a `.fake` file; lines starting with `!`
    /// are reported as skipped records instead.
    struct FakeParser;

    impl SourceParser for FakeParser {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn provider(&self) -> &'static str {
            "acme"
        }
        fn application(&self) -> &'static str {
            "fake-app"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                detect: true,
                parse: true,
                tool_calls: false,
                attachments: false,
                reasoning: false,
                streaming: true,
                partial: false,
            }
        }
        fn description(&self) -> &'static str {
            "test parser"
        }
        fn detect(&self, probe: &Probe) -> Detection {
            if probe.ext() == "fake" {
                Detection::hit(
                    "fake",
                    "acme",
                    "fake-app",
                    "fake",
                    Confidence::Certain,
                    "ext",
                )
            } else {
                Detection::none("fake")
            }
        }
        fn parse(
            &self,
            _ctx: &mut dyn ParseContext,
            source: &Source,
            sink: &mut dyn EventSink,
        ) -> Result<ParseReport> {
            let text = std::fs::read_to_string(&source.read_path)?;
            if text == "!error" {
                anyhow::bail!("intentional parser failure");
            }
            sink.begin(ConversationMeta {
                native_id: Some("11111111-2222-3333-4444-555555555555".into()),
                ..Default::default()
            })?;
            let mut report = ParseReport::default();
            for line in text.lines() {
                if line.starts_with('~') {
                    report.records_failed += 1;
                    continue;
                }
                if line.starts_with('!') {
                    report.records_skipped += 1;
                    continue;
                }
                sink.emit(EventDraft::with_content(
                    crate::model::Role::User,
                    EventType::Message,
                    vec![Part::text(line)],
                ))?;
            }
            sink.end()?;
            Ok(report)
        }
    }

    fn run_import(input: &Path, out: &Path) -> RunStats {
        let opts = ImportOptions {
            output: out.to_path_buf(),
            ..Default::default()
        };
        let mut imp = Importer::new(
            opts,
            Config::default(),
            Registry::new(vec![Box::new(FakeParser)]),
        )
        .unwrap();
        imp.import(&[input.to_path_buf()]).unwrap()
    }

    #[test]
    fn appended_conversation_keeps_its_original_machine_identity() {
        let base = std::env::temp_dir().join(format!(
            "convolith-machine-migration-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let input = base.join("in");
        std::fs::create_dir_all(&input).unwrap();
        let src = input.join("a.fake");
        let out = base.join("out");
        let import = |machine_id: &str| {
            let mut imp = Importer::new(
                ImportOptions {
                    output: out.clone(),
                    machine_id: Some(machine_id.into()),
                    ..Default::default()
                },
                Config::default(),
                Registry::new(vec![Box::new(FakeParser)]),
            )
            .unwrap();
            imp.import(std::slice::from_ref(&input)).unwrap()
        };

        std::fs::write(&src, "old event\n").unwrap();
        assert_eq!(import("linux").events_new, 1);
        let db = rusqlite::Connection::open(out.join("provenance/provenance.sqlite")).unwrap();
        let original_event_id: String = db
            .query_row("select event_id from event_index", [], |r| r.get(0))
            .unwrap();
        std::fs::write(&src, "old event\nappended event\n").unwrap();
        assert_eq!(import("linux/hashed-host").events_new, 1);

        let original_id_still_present: bool = db
            .query_row(
                "select exists(select 1 from event_index where event_id=?1)",
                [&original_event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(original_id_still_present);
        let event_machines: Vec<String> = db
            .prepare("select distinct machine_id from event_index")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(event_machines, vec!["linux"]);
        let conversation_machines: Vec<String> = db
            .prepare("select distinct machine_id from conversation")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(conversation_machines, vec!["linux"]);
        assert_eq!(
            db.query_row("select count(*) from conversation", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn import_accounts_for_every_record_and_reimport_is_all_duplicates() {
        let base = std::env::temp_dir().join(format!("convolith-imp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let input = base.join("in");
        std::fs::create_dir_all(&input).unwrap();
        let src = input.join("a.fake");
        let body = "hello\nworld\n!ignored\nhello\n";
        std::fs::write(&src, body).unwrap();

        let out = base.join("out");
        let first = run_import(&input, &out);
        assert!(first.accounting_holds(), "{first:?}");
        assert_eq!(first.records_examined, 4);
        assert_eq!(
            first.records_imported, 3,
            "same text at another position is not a duplicate"
        );
        assert_eq!(first.records_skipped, 1);
        assert_eq!(first.records_failed, 0);

        let second = run_import(&input, &out);
        assert!(second.accounting_holds(), "{second:?}");
        assert_eq!(second.records_imported, 0);
        assert_eq!(second.records_duplicate, 3);

        // Sources are read-only.
        assert_eq!(std::fs::read_to_string(&src).unwrap(), body);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn partial_and_whole_source_failures_keep_accounting_balanced() {
        let base = std::env::temp_dir().join(format!("convolith-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let input = base.join("in");
        std::fs::create_dir_all(&input).unwrap();
        let partial = input.join("partial.fake");
        std::fs::write(&partial, "ok\n~bad\n").unwrap();
        let partial_stats = run_import(&partial, &base.join("out-partial"));
        assert!(partial_stats.accounting_holds(), "{partial_stats:?}");
        assert_eq!(partial_stats.records_examined, 2);
        assert_eq!(partial_stats.records_imported, 1);
        assert_eq!(partial_stats.records_failed, 1);

        let whole = input.join("whole.fake");
        std::fs::write(&whole, "!error").unwrap();
        let whole_stats = run_import(&whole, &base.join("out-whole"));
        assert!(whole_stats.accounting_holds(), "{whole_stats:?}");
        assert_eq!(whole_stats.records_examined, 1);
        assert_eq!(whole_stats.records_failed, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn chatgpt_fixture_import_is_idempotent() {
        let base = std::env::temp_dir().join(format!("convolith-chatgpt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let input = base.join("input");
        std::fs::create_dir_all(&input).unwrap();
        let source = input.join("conversations.json");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/fixtures/chatgpt/conversations.json"
            ),
            &source,
        )
        .unwrap();
        let output = base.join("output");
        let run = || {
            let mut importer = Importer::new(
                ImportOptions {
                    output: output.clone(),
                    ..Default::default()
                },
                Config::default(),
                crate::parsers::registry(),
            )
            .unwrap();
            importer.import(std::slice::from_ref(&input)).unwrap()
        };
        let first = run();
        assert!(first.accounting_holds(), "{first:?}");
        assert_eq!(first.events_new, 3);
        let second = run();
        assert!(second.accounting_holds(), "{second:?}");
        assert_eq!(second.events_new, 0);
        assert_eq!(second.events_duplicate, 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tiers_are_named() {
        assert_eq!(tier_str(IdentityHint::Native), "native");
        assert_eq!(tier_str(IdentityHint::Coordinates), "coordinates");
        assert_eq!(tier_str(IdentityHint::Fingerprint), "fingerprint");
    }
}
