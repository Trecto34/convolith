//! `convolith all`: one globally chronological, deduplicated JSONL stream of
//! every canonical event in an archive (schema `convolith.all/v1`, see
//! `spec/all-export.md`).
//!
//! Memory: events are streamed from the shards twice (pass 1: native-id →
//! event-id map for parent resolution; pass 2: build lines). Lines are sorted
//! in bounded chunks (`CHUNK_BYTES`), spilled to temp files and k-way merged,
//! so peak memory is ~`CHUNK_BYTES` plus O(events) small id strings (the
//! dedup set and parent map), not the event content.

use crate::dataset::{read_jsonl_zst, Layout};
use crate::model::{Event, Part, ProvenanceRef};
use crate::timeutil::{parse_rfc3339, TimestampConfidence};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = "convolith.all/v1";
const CHUNK_BYTES: usize = 128 * 1024 * 1024;
const MAX_RECORD: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct AllStats {
    pub events: u64,
    pub duplicates_dropped: u64,
    pub unreadable_records: u64,
    pub null_timestamps: u64,
    /// Output `timestamp_confidence` → count.
    pub confidence: BTreeMap<String, u64>,
}

/// (group, ts_ns, confidence rank, tie, seq, event_id). Group 0 = timestamped,
/// 1 = no usable timestamp (placed last; ordered by conversation then source
/// order via `tie`/`seq`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key(u8, i64, u8, String, u64, String);

impl Key {
    fn encode(&self, json: &str) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.0, self.1, self.2, self.3, self.4, self.5, json
        )
    }
    fn decode(line: &str) -> Result<(Key, &str)> {
        let mut p = line.splitn(7, '\t');
        let mut next = || p.next().context("corrupt spill record");
        let k = Key(
            next()?.parse()?,
            next()?.parse()?,
            next()?.parse()?,
            next()?.to_string(),
            next()?.parse()?,
            next()?.to_string(),
        );
        Ok((k, next()?))
    }
}

fn shard_files(layout: &Layout) -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    let dir = layout.events_dir();
    if dir.is_dir() {
        for e in walkdir::WalkDir::new(&dir).sort_by_file_name() {
            let e = e?;
            if e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".jsonl.zst") {
                v.push(e.path().to_path_buf());
            }
        }
    }
    Ok(v)
}

fn for_each_event(
    shards: &[PathBuf],
    stats: &mut AllStats,
    mut f: impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    let mut bad = 0u64;
    for s in shards {
        let (_, b) = read_jsonl_zst::<Event>(s, MAX_RECORD, &mut f, &mut |i, e| {
            eprintln!("convolith all: {}: record {i}: {e}", s.display())
        })?;
        bad += b;
    }
    stats.unreadable_records = bad;
    Ok(())
}

fn opt(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| json!(s))
}

fn block(p: &Part) -> Value {
    fn put(m: &mut Map<String, Value>, k: &str, v: &Option<String>) {
        if let Some(s) = v {
            m.insert(k.into(), json!(s));
        }
    }
    let mut m = Map::new();
    match p {
        Part::Text { text, truncation } => {
            m.insert("type".into(), json!("text"));
            m.insert("text".into(), json!(text));
            if let Some(t) = truncation {
                m.insert("truncation".into(), json!(t));
            }
        }
        Part::Reasoning { text, visibility } => {
            m.insert("type".into(), json!("thinking"));
            m.insert("text".into(), json!(text));
            m.insert("visibility".into(), json!(visibility));
        }
        Part::ToolCall {
            id,
            name,
            arguments,
        } => {
            m.insert("type".into(), json!("tool_call"));
            m.insert("id".into(), id.as_ref().map_or(Value::Null, |s| json!(s)));
            m.insert("name".into(), json!(name));
            m.insert("arguments".into(), arguments.clone());
        }
        Part::ToolResult {
            tool_call_id,
            output,
            is_error,
        } => {
            m.insert("type".into(), json!("tool_result"));
            m.insert(
                "call_id".into(),
                tool_call_id.as_ref().map_or(Value::Null, |s| json!(s)),
            );
            m.insert("output".into(), output.clone());
            m.insert("is_error".into(), json!(is_error));
        }
        Part::Image {
            artifact,
            mime,
            filename,
            source_ref,
        } => {
            m.insert("type".into(), json!("attachment"));
            m.insert("kind".into(), json!("image"));
            put(&mut m, "artifact", artifact);
            put(&mut m, "mime", mime);
            put(&mut m, "filename", filename);
            put(&mut m, "source_ref", source_ref);
        }
        Part::FileRef {
            path,
            artifact,
            mime,
            filename,
        } => {
            m.insert("type".into(), json!("attachment"));
            m.insert("kind".into(), json!("file"));
            put(&mut m, "path", path);
            put(&mut m, "artifact", artifact);
            put(&mut m, "mime", mime);
            put(&mut m, "filename", filename);
        }
        Part::Artifact {
            artifact,
            mime,
            filename,
            size,
        } => {
            m.insert("type".into(), json!("attachment"));
            m.insert("kind".into(), json!("artifact"));
            m.insert("artifact".into(), json!(artifact));
            m.insert("size".into(), json!(size));
            put(&mut m, "mime", mime);
            put(&mut m, "filename", filename);
        }
        Part::Data { value } => {
            m.insert("type".into(), json!("data"));
            m.insert("value".into(), value.clone());
        }
        Part::Opaque { kind, note, raw } => {
            m.insert("type".into(), json!("opaque"));
            m.insert("kind".into(), json!(kind));
            put(&mut m, "note", note);
            if let Some(r) = raw {
                m.insert("raw".into(), r.clone());
            }
        }
    }
    Value::Object(m)
}

/// Subagent / spawn relationship, only from fields the canonical event carries.
fn subagent(md: &Map<String, Value>) -> Value {
    let spawn = md
        .get("source")
        .and_then(|s| s.get("subagent"))
        .and_then(|s| s.get("thread_spawn"));
    let flag = |k: &str| md.get(k).and_then(Value::as_bool) == Some(true);
    let parent_session = md.get("parent_session_id").filter(|v| !v.is_null());
    if !(flag("is_subagent") || flag("is_sidechain") || parent_session.is_some() || spawn.is_some())
    {
        return Value::Null;
    }
    let g = |v: Option<&Value>| v.cloned().unwrap_or(Value::Null);
    json!({
        "is_subagent": true,
        "agent_id": g(md.get("agent_id")),
        "parent_session_id": g(parent_session),
        "parent_thread_id": g(spawn.and_then(|s| s.get("parent_thread_id"))),
        "depth": g(spawn.and_then(|s| s.get("depth"))),
    })
}

struct Ledger {
    conn: Option<Connection>,
}

impl Ledger {
    fn open(layout: &Layout) -> Result<Ledger> {
        let p = layout.provenance_db();
        if !p.is_file() {
            return Ok(Ledger { conn: None });
        }
        let conn = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open ledger {p:?} read-only"))?;
        Ok(Ledger { conn: Some(conn) })
    }

    fn conversations(&self) -> Result<HashMap<String, Value>> {
        let mut out = HashMap::new();
        let Some(c) = &self.conn else { return Ok(out) };
        let mut st = c.prepare(
            "select conversation_id, title, started_at, ended_at, event_count from conversation",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                json!({
                    "title": r.get::<_, Option<String>>(1)?,
                    "started_at": r.get::<_, Option<String>>(2)?,
                    "ended_at": r.get::<_, Option<String>>(3)?,
                    "event_count": r.get::<_, i64>(4)?,
                }),
            ))
        })?;
        for r in rows {
            let (k, v) = r?;
            out.insert(k, v);
        }
        Ok(out)
    }

    fn refs(&self, e: &Event) -> Result<Vec<Value>> {
        let Some(c) = &self.conn else {
            return Ok(e.provenance.iter().map(|p| shard_ref(e, p)).collect());
        };
        let mut st = c.prepare_cached(
            "select o.obs_id, o.source_id, coalesce(s.original_path, o.source_path), o.source_path,
                    o.container_chain, o.record_index, o.record_id, o.parser, o.parser_version,
                    o.source_sha256, o.first_seen, o.import_run, o.identity_tier, s.machine_id
             from observation o left join source s on s.source_id = o.source_id
             where o.event_id = ?1
             order by o.source_id, o.record_index, o.parser, o.obs_id",
        )?;
        let rows = st.query_map([&e.event_id], |r| {
            let chain: String = r.get(4)?;
            Ok(json!({
                "observation_id": r.get::<_, i64>(0)?,
                "source_id": r.get::<_, String>(1)?,
                "original_path": r.get::<_, String>(2)?,
                "source_path": r.get::<_, String>(3)?,
                "container_chain": chain.split("!/").filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                "record_index": r.get::<_, Option<i64>>(5)?,
                "record_id": r.get::<_, Option<String>>(6)?,
                "parser": r.get::<_, String>(7)?,
                "parser_version": r.get::<_, String>(8)?,
                "source_sha256": r.get::<_, Option<String>>(9)?,
                "first_seen": r.get::<_, String>(10)?,
                "import_run": r.get::<_, String>(11)?,
                "identity_tier": r.get::<_, String>(12)?,
                "machine_id": r.get::<_, Option<String>>(13)?,
                "provider": e.provider,
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn shard_ref(e: &Event, p: &ProvenanceRef) -> Value {
    json!({
        "observation_id": null,
        "source_id": p.source_id,
        "original_path": p.source_path,
        "source_path": p.source_path,
        "container_chain": p.container_chain,
        "record_index": p.record_index,
        "record_id": p.record_id,
        "parser": p.parser,
        "parser_version": p.parser_version,
        "source_sha256": p.source_sha256,
        "first_seen": p.first_seen,
        "import_run": p.import_run,
        "identity_tier": p.identity_tier,
        "machine_id": e.machine_id,
        "provider": e.provider,
    })
}

/// Always 9 fractional digits so lexical order of `timestamp` is chronological.
fn fixed_nanos(s: String) -> String {
    match s.strip_suffix('Z') {
        Some(head) if !head.contains('.') => format!("{head}.000000000Z"),
        _ => s,
    }
}

/// (output confidence, rank within it). Ranks: exact < provider < database <
/// filesystem derived < sequence-only < unknown.
fn confidence(c: TimestampConfidence) -> (&'static str, u8) {
    match c {
        TimestampConfidence::Exact => ("exact", 0),
        TimestampConfidence::ProviderDerived => ("derived", 1),
        TimestampConfidence::DatabaseDerived => ("derived", 2),
        TimestampConfidence::FilesystemDerived => ("derived", 3),
        TimestampConfidence::SequenceOnly => ("sequence-only", 4),
        TimestampConfidence::Unknown => ("unknown", 5),
    }
}

fn build(
    e: &Event,
    parents: &HashMap<(String, String), String>,
    convs: &HashMap<String, Value>,
    ledger: &Ledger,
    stats: &mut AllStats,
) -> Result<(Key, String)> {
    // Never invent a time: an unparseable timestamp is treated as absent.
    let ts = e.timestamp.as_deref().and_then(parse_rfc3339);
    let (conf, rank) = confidence(e.timestamp_confidence);
    let (conf, rank) = match (ts, conf) {
        (None, "exact" | "derived") => ("unknown", 5),
        _ => (conf, rank),
    };
    *stats.confidence.entry(conf.into()).or_default() += 1;
    let key = match ts {
        Some(t) => Key(0, t.0, rank, String::new(), e.seq, e.event_id.clone()),
        None => {
            stats.null_timestamps += 1;
            Key(
                1,
                0,
                rank,
                e.conversation_id.clone(),
                e.seq,
                e.event_id.clone(),
            )
        }
    };
    let parent = e.parent_event_id.clone().or_else(|| {
        let n = e.metadata.get("parent_native_id")?.as_str()?;
        parents
            .get(&(e.conversation_id.clone(), n.to_string()))
            .cloned()
    });
    let mut conv = convs
        .get(&e.conversation_id)
        .cloned()
        .unwrap_or(Value::Null);
    if let Value::Object(m) = &mut conv {
        m.insert("first_event".into(), json!(e.seq == 0));
    }
    let line = json!({
        "schema": SCHEMA,
        "event_id": e.event_id,
        "timestamp": ts.map(|t| fixed_nanos(t.to_rfc3339())),
        "timestamp_original": e.timestamp_original,
        "timestamp_confidence": conf,
        "timestamp_confidence_detail": e.timestamp_confidence.as_str(),
        "conversation_id": e.conversation_id,
        "session_id": e.session_id,
        "parent_event_id": parent,
        "seq": e.seq,
        "role": e.role.as_str(),
        "event_type": serde_json::to_value(e.event_type)?,
        "provider": e.provider,
        "application": e.application,
        "model": opt(&e.model),
        "agent": opt(&e.agent),
        "subagent": subagent(&e.metadata),
        "machine_id": opt(&e.machine_id),
        "project_id": opt(&e.project_id),
        "repository_id": opt(&e.repository_id),
        "working_directory": opt(&e.working_directory),
        "branch": opt(&e.branch),
        "commit": opt(&e.commit),
        "worktree_id": opt(&e.worktree_id),
        "conversation": conv,
        "content": e.content.iter().map(block).collect::<Vec<_>>(),
        "metadata": e.metadata,
        "source_refs": ledger.refs(e)?,
    });
    let s = serde_json::to_string(&line)?;
    Ok((key.clone(), key.encode(&s)))
}

/// Sort `chunk` and append its JSON lines to `w`, or spill it to a temp file.
fn spill(chunk: &mut Vec<(Key, String)>, dir: &Path, n: usize) -> Result<PathBuf> {
    chunk.sort_by(|a, b| a.0.cmp(&b.0));
    let p = dir.join(format!("convolith-all-{}-{n}.run", std::process::id()));
    let mut w = BufWriter::new(std::fs::File::create(&p)?);
    for (_, l) in chunk.drain(..) {
        writeln!(w, "{l}")?;
    }
    w.flush()?;
    Ok(p)
}

fn merge(runs: &[PathBuf], w: &mut dyn Write) -> Result<()> {
    let mut readers = Vec::new();
    for p in runs {
        readers.push(BufReader::new(std::fs::File::open(p)?));
    }
    let mut heap = BinaryHeap::new();
    let mut lines = vec![String::new(); readers.len()];
    fn pull(r: &mut BufReader<std::fs::File>, buf: &mut String) -> Result<Option<Key>> {
        buf.clear();
        if r.read_line(buf)? == 0 {
            return Ok(None);
        }
        Ok(Some(Key::decode(buf.trim_end_matches('\n'))?.0))
    }
    for i in 0..readers.len() {
        if let Some(k) = pull(&mut readers[i], &mut lines[i])? {
            heap.push(Reverse((k, i)));
        }
    }
    while let Some(Reverse((_, i))) = heap.pop() {
        let (_, json) = Key::decode(lines[i].trim_end_matches('\n'))?;
        w.write_all(json.as_bytes())?;
        w.write_all(b"\n")?;
        if let Some(k) = pull(&mut readers[i], &mut lines[i])? {
            heap.push(Reverse((k, i)));
        }
    }
    Ok(())
}

/// Write the export to `w`. `tmp_dir` hosts spill files (removed on exit).
pub fn export(archive: &Path, w: &mut dyn Write, tmp_dir: &Path) -> Result<AllStats> {
    let layout = Layout {
        root: archive.to_path_buf(),
    };
    if !layout.is_dataset() {
        bail!("{} is not a convolith archive", archive.display());
    }
    let shards = shard_files(&layout)?;
    let ledger = Ledger::open(&layout)?;
    let convs = ledger.conversations()?;
    let mut stats = AllStats::default();

    let mut parents = HashMap::new();
    for_each_event(&shards, &mut AllStats::default(), |e| {
        if let Some(n) = e.metadata.get("native_id").and_then(Value::as_str) {
            parents
                .entry((e.conversation_id, n.to_string()))
                .or_insert(e.event_id);
        }
        Ok(())
    })?;

    let mut seen = HashSet::new();
    let (mut chunk, mut bytes, mut runs) = (Vec::new(), 0usize, Vec::new());
    let mut dups = 0u64;
    let mut inner = AllStats::default();
    let res = (|| -> Result<()> {
        for_each_event(&shards, &mut stats, |e| {
            if !seen.insert(e.event_id.clone()) {
                dups += 1;
                return Ok(());
            }
            let (k, l) = build(&e, &parents, &convs, &ledger, &mut inner)?;
            bytes += l.len();
            chunk.push((k, l));
            if bytes >= CHUNK_BYTES {
                runs.push(spill(&mut chunk, tmp_dir, runs.len())?);
                bytes = 0;
            }
            Ok(())
        })?;
        if runs.is_empty() {
            chunk.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, l) in &chunk {
                w.write_all(Key::decode(l)?.1.as_bytes())?;
                w.write_all(b"\n")?;
            }
        } else {
            if !chunk.is_empty() {
                runs.push(spill(&mut chunk, tmp_dir, runs.len())?);
            }
            merge(&runs, w)?;
        }
        Ok(())
    })();
    for p in &runs {
        let _ = std::fs::remove_file(p);
    }
    res?;
    stats.events = seen.len() as u64;
    stats.duplicates_dropped = dups;
    stats.null_timestamps = inner.null_timestamps;
    stats.confidence = inner.confidence;
    Ok(stats)
}

/// `--output`: temp file next to the target, then rename.
pub fn export_to_file(archive: &Path, out: &Path) -> Result<AllStats> {
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    let tmp = out.with_file_name(name);
    let res = (|| {
        let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
        let s = export(archive, &mut w, dir)?;
        w.flush()?;
        w.get_ref().sync_all()?;
        Ok(s)
    })();
    match res {
        Ok(s) => {
            std::fs::rename(&tmp, out).with_context(|| format!("rename {tmp:?} -> {out:?}"))?;
            Ok(s)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

pub fn export_to_stdout(archive: &Path) -> Result<AllStats> {
    let out = std::io::stdout();
    let mut w = BufWriter::new(out.lock());
    let s = export(archive, &mut w, &std::env::temp_dir())?;
    w.flush()?;
    Ok(s)
}

impl AllStats {
    pub fn summary(&self) -> String {
        format!(
            "{} event(s), {} duplicate(s) dropped, {} unreadable record(s), {} null timestamp(s); confidence {:?}",
            self.events, self.duplicates_dropped, self.unreadable_records, self.null_timestamps, self.confidence
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_runs_merge_in_key_order() {
        let dir = std::env::temp_dir().join(format!("convolith-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = |ts: i64, id: &str| {
            let k = Key(0, ts, 0, String::new(), 0, id.into());
            (k.clone(), k.encode(&format!("{{\"id\":\"{id}\"}}")))
        };
        let mut a = vec![rec(5, "e"), rec(1, "a"), rec(3, "c")];
        let mut b = vec![rec(4, "d"), rec(2, "b"), rec(3, "c0")];
        let runs = [
            spill(&mut a, &dir, 0).unwrap(),
            spill(&mut b, &dir, 1).unwrap(),
        ];
        let mut out = Vec::new();
        merge(&runs, &mut out).unwrap();
        let ids: Vec<_> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let want = ["a", "b", "c", "c0", "d", "e"].map(|i| format!("{{\"id\":\"{i}\"}}"));
        assert_eq!(ids, want);
        std::fs::remove_dir_all(&dir).ok();
    }
}
