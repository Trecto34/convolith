//! The provenance ledger: `provenance/provenance.sqlite`.
//!
//! This database is the tool's bookkeeping, not the history. It answers three
//! questions cheaply and idempotently:
//!
//! * *have I already seen this event?* — `event_index` keyed by the
//!   deterministic canonical id, so a re-import of identical bytes is a no-op;
//! * *where did it come from?* — `observation`, one row per (source, record);
//! * *what did I fetch and what happened to it?* — `source` and `parse_error`,
//!   which make `examined = imported + duplicates + skipped + failed` checkable.
//!
//! Aggregates (`conversation`, `session`, `project`, `machine`) are maintained
//! incrementally and written out as `data/aggregates/*.jsonl.zst` at the end, so
//! no full-corpus pass is needed to produce them.

use crate::model::{Conversation, Machine, Project, ProvenanceRef, Session};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// How many statements may accumulate before a commit. Bounded memory, batched
/// durability.
const BATCH: u32 = 2_000;

pub const LEDGER_SCHEMA: &str = r#"
create table if not exists meta (key text primary key, value text not null);

create table if not exists source (
    source_id text primary key,
    display_path text not null,
    original_path text not null,
    container_chain text not null default '',
    size integer not null default 0,
    mtime text,
    sha256 text,
    machine_id text,
    platform text,
    provider_guess text,
    application_guess text,
    format text,
    parser text,
    parser_version text,
    detection_confidence text,
    status text not null,
    import_run text not null,
    first_seen text not null,
    last_seen text not null,
    records_examined integer not null default 0,
    records_imported integer not null default 0,
    records_duplicate integer not null default 0,
    records_skipped integer not null default 0,
    records_failed integer not null default 0,
    notes text not null default ''
);
create index if not exists source_path_idx on source(original_path);

-- Resume bookkeeping: a source whose (path, size, mtime, parser, parser
-- version) fingerprint is unchanged and whose last run completed is skipped.
create table if not exists source_state (
    state_key text primary key,
    source_id text not null,
    fingerprint text not null,
    parser text not null,
    parser_version text not null,
    import_run text not null,
    updated_at text not null,
    complete integer not null default 1
);

create table if not exists event_index (
    event_id text primary key,
    conversation_id text not null,
    session_id text,
    seq integer not null,
    role text not null,
    event_type text not null,
    provider text not null,
    application text not null,
    project_id text,
    repository_id text,
    machine_id text,
    timestamp text,
    content_fp text not null,
    identity_tier text not null,
    shard text not null,
    first_seen text not null,
    observation_count integer not null default 0,
    variant_of text
);
create index if not exists event_conversation_idx on event_index(conversation_id, seq);

create table if not exists observation (
    obs_id integer primary key autoincrement,
    event_id text not null,
    source_id text not null,
    source_path text not null,
    container_chain text not null default '',
    record_index integer,
    record_id text,
    parser text not null,
    parser_version text not null,
    source_sha256 text,
    first_seen text not null,
    import_run text not null,
    identity_tier text not null
);
-- `ifnull` is deterministic, so it is legal inside a unique index; this makes
-- re-importing one source record idempotent while still allowing two distinct
-- sources to observe the same event.
create unique index if not exists observation_unique
    on observation(event_id, source_id, ifnull(record_index, -1), parser);
create index if not exists observation_source_idx on observation(source_id);

create table if not exists conflict (
    conflict_id integer primary key autoincrement,
    event_id text not null,
    variant_event_id text not null,
    kind text not null,
    detail text not null,
    content_fp text not null,
    source_id text not null,
    source_path text,
    observed_at text not null
);
create index if not exists conflict_event_idx on conflict(event_id);

create table if not exists parse_error (
    error_id integer primary key autoincrement,
    source_id text not null,
    source_path text,
    locator text,
    message text not null,
    import_run text not null,
    at text not null
);
create index if not exists parse_error_source_idx on parse_error(source_id);

create table if not exists conversation (
    conversation_id text primary key,
    native_id text,
    session_id text,
    provider text not null,
    application text not null,
    title text,
    model text,
    agent text,
    machine_id text,
    project_id text,
    repository_id text,
    working_directory text,
    started_at text,
    ended_at text,
    event_count integer not null default 0,
    identity_tier text not null default 'fingerprint'
);
create table if not exists conversation_source (
    conversation_id text not null,
    source_id text not null,
    primary key (conversation_id, source_id)
);

create table if not exists session (
    session_id text primary key,
    conversation_id text not null,
    provider text not null,
    application text not null,
    title text,
    agent text,
    machine_id text,
    project_id text,
    working_directory text,
    started_at text,
    ended_at text,
    event_count integer not null default 0
);

create table if not exists project (
    project_id text primary key,
    repository_id text,
    name text not null,
    evidence text not null default '',
    event_count integer not null default 0
);
create table if not exists project_path (
    project_id text not null,
    path text not null,
    -- SQLite forbids expressions inside a PRIMARY KEY constraint, so "no
    -- machine known" is spelled as an empty string here instead of `ifnull`.
    machine_id text not null default '',
    primary key (project_id, path, machine_id)
);
create table if not exists project_branch (
    project_id text not null,
    branch text not null,
    primary key (project_id, branch)
);

create table if not exists machine (
    machine_id text primary key,
    platform text,
    event_count integer not null default 0
);
create table if not exists machine_evidence (
    machine_id text not null,
    evidence text not null,
    primary key (machine_id, evidence)
);

create table if not exists artifact_ref (
    artifact_id text primary key,
    sha256 text not null,
    size integer not null,
    mime text,
    filename text,
    source_path text,
    stored_path text not null,
    copied integer not null default 1
);
create table if not exists artifact_event (
    artifact_id text not null,
    event_id text not null,
    primary key (artifact_id, event_id)
);
"#;

pub struct Ledger {
    pub conn: Connection,
    /// Statements written since the last commit.
    pending: u32,
    /// True while a batched write transaction is open. Writes go into one
    /// transaction per batch, which is what makes importing a hundred thousand
    /// events cheap without ever holding the whole corpus in memory.
    in_tx: bool,
}

/// What the caller needs to decide duplicate vs. new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventStatus {
    /// Never seen: write it to the canonical shard.
    New,
    /// Identical identity and content: a duplicate observation.
    Duplicate,
    /// Same identity, different content: a conflict. The variant must be
    /// preserved under `variant_event_id`.
    Conflict { content_fp: String },
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open ledger {path:?}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        // DELETE journal keeps the dataset to one tidy file per database; the
        // ledger is small relative to the event shards it describes.
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(LEDGER_SCHEMA)?;
        let l = Ledger {
            conn,
            pending: 0,
            in_tx: false,
        };
        l.ensure_meta()?;
        Ok(l)
    }

    fn ensure_meta(&self) -> Result<()> {
        self.conn.execute(
            "insert or ignore into meta(key, value) values ('ledger_schema', ?1)",
            [crate::model::SCHEMA_VERSION.to_string()],
        )?;
        self.conn.execute(
            "insert or ignore into meta(key, value) values ('tool', 'convolith')",
            [],
        )?;
        Ok(())
    }

    pub fn schema_version(&self) -> Option<String> {
        self.conn
            .query_row(
                "select value from meta where key='ledger_schema'",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Open a write transaction lazily, so a run that only reads pays nothing.
    fn ensure_tx(&mut self) -> Result<()> {
        if !self.in_tx {
            self.conn.execute_batch("begin immediate;")?;
            self.in_tx = true;
        }
        Ok(())
    }

    /// Cheap write batching: commit every [`BATCH`] statements so an
    /// interruption loses at most one batch.
    fn tick(&mut self) -> Result<()> {
        self.ensure_tx()?;
        self.pending += 1;
        if self.pending >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.in_tx && self.pending > 0 {
            self.conn.execute_batch("commit;")?;
            self.in_tx = false;
            self.pending = 0;
        }
        Ok(())
    }

    /// Commit any open batch and leave the ledger consistent.
    pub fn finish(&mut self) -> Result<()> {
        self.flush()?;
        // Keep the file compact and self-contained.
        let _ = self.conn.execute_batch("optimize;");
        Ok(())
    }

    // ---- source bookkeeping -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn record_source(&mut self, s: &SourceRecord) -> Result<()> {
        let chain = s.container_chain.join("!/");
        self.conn.execute(
            "insert into source(source_id, display_path, original_path, container_chain, size,
                 mtime, sha256, machine_id, platform, provider_guess, application_guess, format,
                 parser, parser_version, detection_confidence, status, import_run, first_seen,
                 last_seen, records_examined, records_imported, records_duplicate,
                 records_skipped, records_failed, notes)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,
                 ?20,?21,?22,?23,?24,?25)
             on conflict(source_id) do update set
                 last_seen = excluded.last_seen,
                 import_run = excluded.import_run,
                 status = excluded.status,
                 provider_guess = coalesce(source.provider_guess, excluded.provider_guess),
                 application_guess = coalesce(source.application_guess, excluded.application_guess),
                 format = case when source.parser is null then coalesce(excluded.format, source.format)
                               else source.format end,
                 parser = coalesce(source.parser, excluded.parser),
                 parser_version = coalesce(source.parser_version, excluded.parser_version),
                 detection_confidence = coalesce(source.detection_confidence, excluded.detection_confidence),
                 records_examined = source.records_examined + excluded.records_examined,
                 records_imported = source.records_imported + excluded.records_imported,
                 records_duplicate = source.records_duplicate + excluded.records_duplicate,
                 records_skipped = source.records_skipped + excluded.records_skipped,
                 records_failed = source.records_failed + excluded.records_failed,
                 notes = case when excluded.notes = '' then source.notes
                              when source.notes = '' then excluded.notes
                              else source.notes || char(10) || excluded.notes end",
            params![
                s.source_id,
                s.display_path,
                s.original_path,
                chain,
                s.size,
                s.mtime,
                s.sha256,
                s.machine_id,
                s.platform,
                s.provider_guess,
                s.application_guess,
                s.format,
                s.parser,
                s.parser_version,
                s.detection_confidence,
                s.status,
                s.import_run,
                s.first_seen,
                s.last_seen,
                s.records_examined,
                s.records_imported,
                s.records_duplicate,
                s.records_skipped,
                s.records_failed,
                s.notes,
            ],
        )?;
        self.tick()
    }

    /// Mark a source complete for resume purposes.
    #[allow(clippy::too_many_arguments)]
    pub fn mark_source_complete(
        &mut self,
        state_key: &str,
        source_id: &str,
        fingerprint: &str,
        parser: &str,
        parser_version: &str,
        import_run: &str,
        at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "insert into source_state(state_key, source_id, fingerprint, parser, parser_version,
                 import_run, updated_at, complete)
             values (?1,?2,?3,?4,?5,?6,?7,1)
             on conflict(state_key) do update set
                 source_id=excluded.source_id, fingerprint=excluded.fingerprint,
                 parser=excluded.parser, parser_version=excluded.parser_version,
                 import_run=excluded.import_run, updated_at=excluded.updated_at, complete=1",
            params![
                state_key,
                source_id,
                fingerprint,
                parser,
                parser_version,
                import_run,
                at
            ],
        )?;
        self.tick()
    }

    pub fn is_source_complete(
        &self,
        state_key: &str,
        fingerprint: &str,
        parser: &str,
        parser_version: &str,
    ) -> bool {
        self.conn
            .query_row(
                "select 1 from source_state where state_key=?1 and fingerprint=?2 and parser=?3
                   and parser_version=?4 and complete=1",
                params![state_key, fingerprint, parser, parser_version],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn record_parse_error(
        &mut self,
        source_id: &str,
        source_path: Option<&str>,
        locator: Option<&str>,
        message: &str,
        import_run: &str,
        at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "insert into parse_error(source_id, source_path, locator, message, import_run, at)
             values (?1,?2,?3,?4,?5,?6)",
            params![source_id, source_path, locator, message, import_run, at],
        )?;
        self.tick()
    }

    // ---- event index + observations ----------------------------------------

    /// Classify an incoming event without writing it.
    pub fn check_event(&self, event_id: &str, content_fp: &str) -> Result<EventStatus> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "select content_fp, variant_of from event_index where event_id=?1",
                [event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            None => EventStatus::New,
            Some((fp, _)) if fp == content_fp => EventStatus::Duplicate,
            Some((fp, _)) => EventStatus::Conflict { content_fp: fp },
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_event(
        &mut self,
        event_id: &str,
        conversation_id: &str,
        session_id: Option<&str>,
        seq: u64,
        role: &str,
        event_type: &str,
        provider: &str,
        application: &str,
        project_id: Option<&str>,
        repository_id: Option<&str>,
        machine_id: Option<&str>,
        timestamp: Option<&str>,
        content_fp: &str,
        identity_tier: &str,
        shard: &str,
        first_seen: &str,
        variant_of: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "insert or ignore into event_index(event_id, conversation_id, session_id, seq, role,
                 event_type, provider, application, project_id, repository_id, machine_id,
                 timestamp, content_fp, identity_tier, shard, first_seen, observation_count,
                 variant_of)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,0,?17)",
            params![
                event_id,
                conversation_id,
                session_id,
                seq as i64,
                role,
                event_type,
                provider,
                application,
                project_id,
                repository_id,
                machine_id,
                timestamp,
                content_fp,
                identity_tier,
                shard,
                first_seen,
                variant_of,
            ],
        )?;
        self.tick()
    }

    /// Add one provenance observation. Returns true when it is new, so the
    /// caller can keep `observation_count` honest.
    #[allow(clippy::too_many_arguments)]
    pub fn add_observation(
        &mut self,
        event_id: &str,
        p: &ProvenanceRef,
        source_id: &str,
    ) -> Result<bool> {
        let chain = p.container_chain.join("!/");
        let n = self.conn.execute(
            "insert or ignore into observation(event_id, source_id, source_path, container_chain,
                 record_index, record_id, parser, parser_version, source_sha256, first_seen,
                 import_run, identity_tier)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                event_id,
                source_id,
                p.source_path,
                chain,
                p.record_index.map(|v| v as i64),
                p.record_id,
                p.parser,
                p.parser_version,
                p.source_sha256,
                p.first_seen,
                p.import_run,
                p.identity_tier,
            ],
        )?;
        if n > 0 {
            self.conn.execute(
                "update event_index set observation_count = observation_count + 1 where event_id=?1",
                [event_id],
            )?;
            self.tick()?;
            return Ok(true);
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_conflict(
        &mut self,
        event_id: &str,
        variant_event_id: &str,
        kind: &str,
        detail: &str,
        content_fp: &str,
        source_id: &str,
        source_path: Option<&str>,
        at: &str,
    ) -> Result<bool> {
        // Idempotent: re-importing the same conflicting pair adds no row.
        let seen: i64 = self.conn.query_row(
            "select count(*) from conflict where event_id = ?1 and variant_event_id = ?2
                 and content_fp = ?3 and source_id = ?4",
            params![event_id, variant_event_id, content_fp, source_id],
            |r| r.get(0),
        )?;
        if seen > 0 {
            return Ok(false);
        }
        self.conn.execute(
            "insert into conflict(event_id, variant_event_id, kind, detail, content_fp, source_id,
                 source_path, observed_at)
             values (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                event_id,
                variant_event_id,
                kind,
                detail,
                content_fp,
                source_id,
                source_path,
                at
            ],
        )?;
        self.tick()?;
        Ok(true)
    }

    // ---- aggregates --------------------------------------------------------

    pub fn upsert_conversation(&mut self, c: &Conversation, tier: &str) -> Result<()> {
        self.conn.execute(
            "insert into conversation(conversation_id, native_id, session_id, provider,
                 application, title, model, agent, machine_id, project_id, repository_id,
                 working_directory, started_at, ended_at, event_count, identity_tier)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             on conflict(conversation_id) do update set
                 title = coalesce(conversation.title, excluded.title),
                 model = coalesce(conversation.model, excluded.model),
                 agent = coalesce(conversation.agent, excluded.agent),
                 machine_id = coalesce(conversation.machine_id, excluded.machine_id),
                 project_id = coalesce(conversation.project_id, excluded.project_id),
                 repository_id = coalesce(conversation.repository_id, excluded.repository_id),
                 working_directory = coalesce(conversation.working_directory, excluded.working_directory),
                 started_at = min(coalesce(conversation.started_at, excluded.started_at),
                                  coalesce(excluded.started_at, conversation.started_at)),
                 ended_at = max(coalesce(conversation.ended_at, excluded.ended_at),
                                coalesce(excluded.ended_at, conversation.ended_at))",
            params![
                c.conversation_id,
                c.session_id,
                c.session_id,
                c.provider,
                c.application,
                c.title,
                c.model,
                c.agent,
                c.machine_id,
                c.project_id,
                c.repository_id,
                c.working_directory,
                c.started_at,
                c.ended_at,
                c.event_count as i64,
                tier,
            ],
        )?;
        for sid in &c.source_ids {
            self.conn.execute(
                "insert or ignore into conversation_source(conversation_id, source_id) values (?1,?2)",
                params![c.conversation_id, sid],
            )?;
        }
        self.tick()
    }

    pub fn bump_conversation(&mut self, conversation_id: &str, seq: u64) -> Result<()> {
        self.conn.execute(
            "update conversation set event_count = event_count + 1 where conversation_id=?1",
            [conversation_id],
        )?;
        let _ = seq;
        self.tick()
    }

    pub fn conversation_machine_id(&self, conversation_id: &str) -> Result<Option<String>> {
        let machine_id = self
            .conn
            .query_row(
                "select machine_id from conversation where conversation_id=?1",
                [conversation_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(machine_id.flatten())
    }

    pub fn upsert_session(&mut self, s: &Session) -> Result<()> {
        self.conn.execute(
            "insert into session(session_id, conversation_id, provider, application, title, agent,
                 machine_id, project_id, working_directory, started_at, ended_at, event_count)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             on conflict(session_id) do update set
                 event_count = session.event_count + excluded.event_count",
            params![
                s.session_id,
                s.conversation_id,
                s.provider,
                s.application,
                s.title,
                s.agent,
                s.machine_id,
                s.project_id,
                s.working_directory,
                s.started_at,
                s.ended_at,
                s.event_count as i64,
            ],
        )?;
        self.tick()
    }

    pub fn upsert_project(
        &mut self,
        p: &Project,
        path: Option<&str>,
        machine: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "insert into project(project_id, repository_id, name, evidence, event_count)
             values (?1,?2,?3,?4,?5)
             on conflict(project_id) do update set
                 repository_id = coalesce(project.repository_id, excluded.repository_id),
                 name = case when excluded.name = '' then project.name else excluded.name end",
            params![
                p.project_id,
                p.repository_id,
                p.name,
                p.evidence.join("; "),
                p.event_count as i64
            ],
        )?;
        if let Some(path) = path {
            self.conn.execute(
                "insert or ignore into project_path(project_id, path, machine_id) values (?1,?2,?3)",
                params![p.project_id, path, machine.unwrap_or("")],
            )?;
        }
        for b in &p.branches {
            self.conn.execute(
                "insert or ignore into project_branch(project_id, branch) values (?1,?2)",
                params![p.project_id, b],
            )?;
        }
        self.tick()
    }

    pub fn upsert_machine(&mut self, m: &Machine) -> Result<()> {
        self.conn.execute(
            "insert into machine(machine_id, platform, event_count) values (?1,?2,?3)
             on conflict(machine_id) do update set
                 platform = coalesce(machine.platform, excluded.platform)",
            params![m.machine_id, m.platform, m.event_count as i64],
        )?;
        for e in &m.evidence {
            self.conn.execute(
                "insert or ignore into machine_evidence(machine_id, evidence) values (?1,?2)",
                params![m.machine_id, e],
            )?;
        }
        self.tick()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_artifact(
        &mut self,
        artifact_id: &str,
        sha256: &str,
        size: u64,
        mime: Option<&str>,
        filename: Option<&str>,
        source_path: Option<&str>,
        stored_path: &str,
        copied: bool,
        event_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "insert or ignore into artifact_ref(artifact_id, sha256, size, mime, filename,
                 source_path, stored_path, copied)
             values (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                artifact_id,
                sha256,
                size as i64,
                mime,
                filename,
                source_path,
                stored_path,
                copied as i64
            ],
        )?;
        if !event_id.is_empty() {
            self.conn.execute(
                "insert or ignore into artifact_event(artifact_id, event_id) values (?1,?2)",
                params![artifact_id, event_id],
            )?;
        }
        self.tick()
    }

    // ---- read helpers used by reports, stats and the writers ---------------

    /// Parse errors, oldest first, for the reports.
    pub fn parse_errors(&self) -> Result<Vec<(String, Option<String>, String)>> {
        let mut stmt = self
            .conn
            .prepare("select source_path, locator, message from parse_error order by error_id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn count(&self, sql: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(sql, [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    pub fn scalar(&self, sql: &str) -> Result<Option<String>> {
        let v: Option<Option<String>> = self.conn.query_row(sql, [], |r| r.get(0)).optional()?;
        Ok(v.flatten())
    }

    pub fn date_range(&self) -> Result<Option<(String, String)>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "select min(timestamp), max(timestamp) from event_index where timestamp is not null",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(a), Some(b))) => Some((a, b)),
            _ => None,
        })
    }

    /// Stream every indexed event id with its shard, used by `validate` to
    /// check that the canonical data and the ledger still agree.
    pub fn for_each_event(&self, mut f: impl FnMut(&str, &str) -> Result<()>) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("select event_id, shard from event_index order by event_id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let shard: String = r.get(1)?;
            f(&id, &shard)?;
        }
        Ok(())
    }

    /// Rebuild the aggregate tables from the event index and the observations.
    /// `project_branch` and `machine_evidence` are import-time facts that no ledger
    /// table records, so they are left as-is.
    pub fn rebuild_aggregates(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "delete from conversation_source;
             delete from project_path;
             update conversation set event_count = (
                 select count(*) from event_index e where e.conversation_id = conversation.conversation_id);
             update project set event_count = (
                 select count(*) from event_index e where e.project_id = project.project_id);
             update machine set event_count = (
                 select count(*) from event_index e where e.machine_id = machine.machine_id);
             insert or ignore into conversation_source(conversation_id, source_id)
                 select distinct conversation_id, source_id from observation o
                 join event_index e on e.event_id = o.event_id;
             insert or ignore into project_path(project_id, path, machine_id)
                 select distinct c.project_id, c.working_directory, ifnull(c.machine_id, '')
                 from conversation c
                 where c.project_id is not null and c.working_directory is not null;",
        )?;
        Ok(())
    }

    pub fn provenance_for(&self, event_id: &str) -> Result<Vec<ProvenanceRow>> {
        let mut stmt = self.conn.prepare(
            "select o.source_id, o.source_path, o.container_chain, o.record_index, o.record_id,
                    o.parser, o.parser_version, o.source_sha256, o.first_seen, o.import_run,
                    o.identity_tier, s.machine_id, s.platform, s.format, s.status
             from observation o left join source s on s.source_id = o.source_id
             where o.event_id = ?1
             order by o.source_id, o.record_index",
        )?;
        let rows = stmt.query_map([event_id], |r| {
            Ok(ProvenanceRow {
                source_id: r.get(0)?,
                source_path: r.get(1)?,
                container_chain: r
                    .get::<_, String>(2)?
                    .split("!/")
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                record_index: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                record_id: r.get(4)?,
                parser: r.get(5)?,
                parser_version: r.get(6)?,
                source_sha256: r.get(7)?,
                first_seen: r.get(8)?,
                import_run: r.get(9)?,
                identity_tier: r.get(10)?,
                machine_id: r.get(11)?,
                platform: r.get(12)?,
                format: r.get(13)?,
                source_status: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl Ledger {
    /// Rebuild `Conversation` aggregates from the ledger itself.
    pub fn aggregate_conversations(&self) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "select conversation_id, session_id, provider, application, title, model, agent,
                    machine_id, project_id, repository_id, working_directory, started_at,
                    ended_at, event_count
             from conversation order by conversation_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Conversation {
                schema_version: crate::model::SCHEMA_VERSION,
                conversation_id: r.get(0)?,
                session_id: r.get(1)?,
                provider: r.get(2)?,
                application: r.get(3)?,
                title: r.get(4)?,
                model: r.get(5)?,
                agent: r.get(6)?,
                machine_id: r.get(7)?,
                project_id: r.get(8)?,
                repository_id: r.get(9)?,
                working_directory: r.get(10)?,
                started_at: r.get(11)?,
                ended_at: r.get(12)?,
                event_count: r.get::<_, i64>(13)?.max(0) as u64,
                source_ids: self
                    .sources_for_conversation(&r.get::<_, String>(0)?)
                    .unwrap_or_default(),
                metadata: serde_json::Map::new(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn sources_for_conversation(&self, id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("select source_id from conversation_source where conversation_id=?1")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn aggregate_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "select session_id, conversation_id, provider, application, title, agent, machine_id,
                    project_id, working_directory, started_at, ended_at, event_count
             from session order by session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Session {
                schema_version: crate::model::SCHEMA_VERSION,
                session_id: r.get(0)?,
                conversation_id: r.get(1)?,
                provider: r.get(2)?,
                application: r.get(3)?,
                title: r.get(4)?,
                agent: r.get(5)?,
                machine_id: r.get(6)?,
                project_id: r.get(7)?,
                working_directory: r.get(8)?,
                started_at: r.get(9)?,
                ended_at: r.get(10)?,
                event_count: r.get::<_, i64>(11)?.max(0) as u64,
                source_ids: Vec::new(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn aggregate_projects(&self) -> Result<Vec<Project>> {
        let mut projects: Vec<Project> = {
            let mut stmt = self.conn.prepare(
                "select project_id, repository_id, name, evidence, event_count from project order by project_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Project {
                    project_id: r.get(0)?,
                    repository_id: r.get(1)?,
                    name: r.get(2)?,
                    paths: Vec::new(),
                    machines: Vec::new(),
                    branches: Vec::new(),
                    evidence: r
                        .get::<_, String>(3)?
                        .split("; ")
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                    event_count: r.get::<_, i64>(4)?.max(0) as u64,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for p in projects.iter_mut() {
            let mut stmt = self.conn.prepare(
                "select path, ifnull(machine_id,'') from project_path where project_id=?1",
            )?;
            let rows = stmt.query_map([&p.project_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (path, machine) = row?;
                p.paths.push(path);
                if !machine.is_empty() && !p.machines.contains(&machine) {
                    p.machines.push(machine);
                }
            }
            p.paths.sort();
            p.machines.sort();
            let mut stmt = self
                .conn
                .prepare("select branch from project_branch where project_id=?1 order by branch")?;
            let rows = stmt.query_map([&p.project_id], |r| r.get::<_, String>(0))?;
            for row in rows {
                p.branches.push(row?);
            }
        }
        Ok(projects)
    }

    pub fn aggregate_machines(&self) -> Result<Vec<Machine>> {
        let mut machines: Vec<Machine> = {
            let mut stmt = self.conn.prepare(
                "select machine_id, platform, event_count from machine order by machine_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Machine {
                    machine_id: r.get(0)?,
                    platform: r.get(1)?,
                    evidence: Vec::new(),
                    source_ids: Vec::new(),
                    event_count: r.get::<_, i64>(2)?.max(0) as u64,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for m in machines.iter_mut() {
            let mut stmt = self.conn.prepare(
                "select evidence from machine_evidence where machine_id=?1 order by evidence",
            )?;
            let rows = stmt.query_map([&m.machine_id], |r| r.get::<_, String>(0))?;
            for row in rows {
                m.evidence.push(row?);
            }
        }
        Ok(machines)
    }

    pub fn aggregate_artifacts(&self) -> Result<Vec<crate::model::ArtifactRecord>> {
        let mut stmt = self.conn.prepare(
            "select artifact_id, sha256, size, mime, filename, source_path, stored_path, copied
             from artifact_ref order by artifact_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::model::ArtifactRecord {
                schema_version: crate::model::SCHEMA_VERSION,
                artifact_id: r.get(0)?,
                sha256: r.get(1)?,
                size: r.get::<_, i64>(2)?.max(0) as u64,
                mime: r.get(3)?,
                filename: r.get(4)?,
                source_path: r.get(5)?,
                event_ids: Vec::new(),
                stored_path: r.get(6)?,
                copied: r.get::<_, i64>(7)? != 0,
            })
        })?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        let mut ev = self.conn.prepare(
            "select event_id from artifact_event where artifact_id = ?1 and event_id != '' order by event_id",
        )?;
        for a in &mut out {
            a.event_ids = ev
                .query_map([&a.artifact_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
        }
        Ok(out)
    }

    /// Conflicting pairs, for `reports/CONFLICTS.md`.
    #[allow(clippy::type_complexity)]
    pub fn conflicts(&self) -> Result<Vec<(String, String, String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "select event_id, variant_event_id, kind, detail, source_path from conflict
             order by event_id, conflict_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Events with more than one observation: the duplicate ledger.
    pub fn multi_observed(&self, limit: u64) -> Result<Vec<(String, u64, String)>> {
        let mut stmt = self.conn.prepare(
            "select e.event_id, e.observation_count,
                    (select group_concat(p, ' | ') from (
                        select distinct s.original_path as p from observation o
                          join source s on s.source_id = o.source_id
                         where o.event_id = e.event_id))
             from event_index e where e.observation_count > 1
             order by e.observation_count desc, e.event_id limit ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.max(0) as u64,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn observation_count(&self) -> Result<u64> {
        self.count("select count(*) from observation")
    }

    #[allow(clippy::type_complexity)]
    pub fn source_rows(&self) -> Result<Vec<(String, String, String, u64, u64, u64, u64, u64)>> {
        let mut stmt = self.conn.prepare(
            "select source_id, original_path, status, records_examined, records_imported,
                    records_duplicate, records_skipped, records_failed
             from source order by original_path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?.max(0) as u64,
                r.get::<_, i64>(4)?.max(0) as u64,
                r.get::<_, i64>(5)?.max(0) as u64,
                r.get::<_, i64>(6)?.max(0) as u64,
                r.get::<_, i64>(7)?.max(0) as u64,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProvenanceRow {
    pub source_id: String,
    pub source_path: String,
    pub container_chain: Vec<String>,
    pub record_index: Option<u64>,
    pub record_id: Option<String>,
    pub parser: String,
    pub parser_version: String,
    pub source_sha256: Option<String>,
    pub first_seen: String,
    pub import_run: String,
    pub identity_tier: String,
    pub machine_id: Option<String>,
    pub platform: Option<String>,
    pub format: Option<String>,
    pub source_status: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SourceRecord {
    pub source_id: String,
    pub display_path: String,
    pub original_path: String,
    pub container_chain: Vec<String>,
    pub size: u64,
    pub mtime: Option<String>,
    pub sha256: Option<String>,
    pub machine_id: Option<String>,
    pub platform: Option<String>,
    pub provider_guess: Option<String>,
    pub application_guess: Option<String>,
    pub format: Option<String>,
    pub parser: Option<String>,
    pub parser_version: Option<String>,
    pub detection_confidence: Option<String>,
    pub status: String,
    pub import_run: String,
    pub first_seen: String,
    pub last_seen: String,
    pub records_examined: u64,
    pub records_imported: u64,
    pub records_duplicate: u64,
    pub records_skipped: u64,
    pub records_failed: u64,
    pub notes: String,
}

/// Open a write batch explicitly. Callers use this around a whole source so an
/// interruption loses at most that source; the importer also relies on the
/// implicit batching in [`Ledger::tick`] for the per-event path.
pub fn begin(ledger: &mut Ledger) -> Result<()> {
    ledger.ensure_tx()
}

/// Commit the current batch, if any.
pub fn commit(ledger: &mut Ledger) -> Result<()> {
    ledger.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (Ledger, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "convolith-ledger-{}-{}",
            std::process::id(),
            crate::timeutil::now_utc().0
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let l = Ledger::open(&dir.join("p.sqlite")).unwrap();
        (l, dir)
    }

    #[test]
    fn duplicate_and_conflict_are_distinguished() {
        let (mut l, dir) = ledger();
        assert_eq!(l.check_event("ev_1", "fpA").unwrap(), EventStatus::New);
        l.insert_event(
            "ev_1",
            "cv_1",
            None,
            0,
            "user",
            "message",
            "p",
            "a",
            None,
            None,
            None,
            None,
            "fpA",
            "native",
            "data/events/part-000001.jsonl.zst",
            "t0",
            None,
        )
        .unwrap();
        assert_eq!(
            l.check_event("ev_1", "fpA").unwrap(),
            EventStatus::Duplicate
        );
        assert_eq!(
            l.check_event("ev_1", "fpB").unwrap(),
            EventStatus::Conflict {
                content_fp: "fpA".into()
            }
        );
        l.finish().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observations_are_idempotent_per_source_record() {
        let (mut l, dir) = ledger();
        let p = ProvenanceRef {
            source_id: "src_1".into(),
            source_path: "a.jsonl".into(),
            container_chain: vec!["backup.tar.gz".into(), "a.jsonl".into()],
            record_index: Some(3),
            record_id: None,
            parser: "generic_jsonl".into(),
            parser_version: "1".into(),
            source_sha256: None,
            first_seen: "t0".into(),
            import_run: "run1".into(),
            identity_tier: "coordinates".into(),
        };
        l.insert_event(
            "ev_1",
            "cv_1",
            None,
            0,
            "user",
            "message",
            "p",
            "a",
            None,
            None,
            None,
            None,
            "fp",
            "coordinates",
            "s",
            "t0",
            None,
        )
        .unwrap();
        assert!(l.add_observation("ev_1", &p, "src_1").unwrap());
        assert!(
            !l.add_observation("ev_1", &p, "src_1").unwrap(),
            "same record must not double-count"
        );
        // A different record in the same source is a new observation.
        let mut p2 = p.clone();
        p2.record_index = Some(4);
        assert!(l.add_observation("ev_1", &p2, "src_1").unwrap());
        let n = l
            .count("select count(*) from event_index where event_id='ev_1'")
            .unwrap();
        assert_eq!(n, 1);
        let obs = l
            .count("select count(*) from observation where event_id='ev_1'")
            .unwrap();
        assert_eq!(obs, 2, "one event, two observations");
        let fetched = l.provenance_for("ev_1").unwrap();
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].container_chain, vec!["backup.tar.gz", "a.jsonl"]);
        l.finish().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_state_roundtrips() {
        let (mut l, dir) = ledger();
        assert!(!l.is_source_complete("k", "fp", "p", "1"));
        l.mark_source_complete("k", "src", "fp", "p", "1", "run1", "t")
            .unwrap();
        assert!(l.is_source_complete("k", "fp", "p", "1"));
        assert!(
            !l.is_source_complete("k", "fp", "p", "2"),
            "a parser bump re-parses"
        );
        assert!(
            !l.is_source_complete("k", "other", "p", "1"),
            "changed bytes re-parse"
        );
        l.finish().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
