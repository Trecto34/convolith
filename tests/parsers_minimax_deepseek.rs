//! Acceptance tests for the `minimax` and `deepseek` parsers against the
//! synthetic fixtures in `fixtures/{minimax,deepseek}` (no real chat content).

use convolith::config::Config;
use convolith::discover::probe_file;
use convolith::importer::{ImportOptions, Importer};
use convolith::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use convolith::parser::{ConversationMeta, EventSink, ParseContext, SourceParser};
use convolith::parsers::{deepseek::DeepSeekParser, minimax::MiniMaxParser, registry};
use convolith::secrets::RedactionHit;
use convolith::source::{Confidence, ParseReport, Source};
use std::path::{Path, PathBuf};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
const MM_DIR: &str = "minimax/.minimax/v2/sessions/2026/10/01/10-00-00-000-session_bXZzXzAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVm";
const MM_SESSION: &str = "mvs_0123456789abcdef0123456789abcdef";
const DS_DIR: &str =
    "deepseek/.dsh/sessions/--home-demo-proj--/session-5c1f2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f";

// ---------------------------------------------------------------- harness

#[derive(Default)]
struct Collect {
    metas: Vec<ConversationMeta>,
    events: Vec<EventDraft>,
    ended: usize,
}

impl EventSink for Collect {
    fn begin(&mut self, meta: ConversationMeta) -> anyhow::Result<()> {
        assert_eq!(
            self.metas.len(),
            self.ended,
            "begin before the previous end"
        );
        self.metas.push(meta);
        Ok(())
    }
    fn emit(&mut self, draft: EventDraft) -> anyhow::Result<()> {
        assert!(self.metas.len() > self.ended, "emit outside begin/end");
        self.events.push(draft);
        Ok(())
    }
    fn end(&mut self) -> anyhow::Result<()> {
        self.ended += 1;
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
        "test"
    }
    fn provider(&self) -> &str {
        "test"
    }
    fn application(&self) -> &str {
        "test"
    }
    fn parser_id(&self) -> &str {
        "test"
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
        anyhow::bail!("parsers under test must not store artifacts")
    }
    fn apply_secret_policy(&self, _: &str, text: &str) -> (String, Option<RedactionHit>) {
        (text.to_string(), None)
    }
    fn max_record_bytes(&self) -> usize {
        self.max_record
    }
    fn note(&mut self, _: String) {}
}

fn source(path: &Path) -> Source {
    Source {
        source_id: "src_test".into(),
        display_path: path.display().to_string(),
        container_chain: vec![],
        read_path: path.to_path_buf(),
        inner_path: None,
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        mtime: None,
        sha256: None,
        machine_id: None,
        platform: None,
        provider_label: None,
        application_label: None,
        parser_id: None,
    }
}

fn parse_with(
    p: &dyn SourceParser,
    path: &Path,
    max_record: usize,
) -> anyhow::Result<(Collect, ParseReport)> {
    let mut sink = Collect::default();
    let mut ctx = Ctx {
        staging: std::env::temp_dir(),
        max_record,
    };
    let report = p.parse(&mut ctx, &source(path), &mut sink)?;
    assert_eq!(sink.metas.len(), sink.ended, "every begin has an end");
    Ok((sink, report))
}

fn parse_file(p: &dyn SourceParser, path: &Path) -> (Collect, ParseReport) {
    let (sink, report) = parse_with(p, path, 64 * 1024 * 1024).unwrap();
    let lines = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count() as u64;
    assert_eq!(
        report.records_examined, lines,
        "examined == non-blank lines"
    );
    assert_eq!(
        sink.events.len() as u64 + report.records_skipped + report.records_failed,
        lines,
        "events + skipped + failed == non-blank lines"
    );
    (sink, report)
}

fn mm(rel: &str) -> PathBuf {
    Path::new(FIX).join(MM_DIR).join(rel)
}
fn ds(rel: &str) -> PathBuf {
    Path::new(FIX).join(DS_DIR).join(rel)
}
fn by_native<'a>(c: &'a Collect, id: &str) -> &'a EventDraft {
    c.events
        .iter()
        .find(|e| e.native_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no event {id}"))
}
fn text(e: &EventDraft) -> String {
    e.content
        .iter()
        .filter_map(|p| p.as_text())
        .collect::<Vec<_>>()
        .join("")
}
fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-mmds-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Import `path` into a fresh archive; returns (archive, event count, sources).
fn import(path: &Path, out: &Path) -> (u64, Vec<(String, String, String)>) {
    let opts = ImportOptions {
        output: out.to_path_buf(),
        ..Default::default()
    };
    let mut imp = Importer::new(opts, Config::default(), registry()).unwrap();
    imp.import(&[path.to_path_buf()]).unwrap();
    let db = rusqlite::Connection::open(out.join("provenance/provenance.sqlite")).unwrap();
    let events: u64 = db
        .query_row("select count(*) from event_index", [], |r| r.get(0))
        .unwrap();
    let mut st = db
        .prepare("select display_path, parser, application_guess from source where status != 'unsupported' order by 1")
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    (events, rows)
}

// ------------------------------------------------------------------ minimax

#[test]
fn minimax_detects_history_files_only() {
    let p = MiniMaxParser;
    let live = probe_file(&mm("messages.jsonl")).unwrap();
    let d = p.detect(&live);
    assert!(d.is_hit());
    assert_eq!(d.format, "minimax-session-jsonl");
    assert_eq!(
        d.confidence,
        Confidence::Certain,
        "under .minimax/.../sessions"
    );
    let snap = mm("snapshots/g000000000000--ctx_11111111-2222-3333-4444-555555555555.jsonl");
    assert!(p.detect(&probe_file(&snap).unwrap()).is_hit());
    // Bookkeeping files and a look-alike outside the store are not claimed.
    for other in [
        "manifest.json",
        "llm-call.json",
        "user-message-locators.jsonl",
    ] {
        let probe = probe_file(&mm(other)).unwrap();
        assert!(!p.detect(&probe).is_hit(), "{other}");
        let (format, reason) = convolith::parsers::known_unsupported(&probe).unwrap();
        assert_eq!(format, "minimax-session-metadata");
        assert!(reason.contains("no messages"));
    }
    let dir = tmp("mm-detect");
    let stray = dir.join("messages.jsonl");
    std::fs::copy(mm("messages.jsonl"), &stray).unwrap();
    assert_eq!(
        p.detect(&probe_file(&stray).unwrap()).confidence,
        Confidence::Strong
    );
    std::fs::write(dir.join("notes.jsonl"), "{\"message_id\":\"m\"}\n").unwrap();
    assert!(!p
        .detect(&probe_file(&dir.join("notes.jsonl")).unwrap())
        .is_hit());
}

#[test]
fn minimax_session_maps_roles_blocks_and_ids() {
    let (c, r) = parse_file(&MiniMaxParser, &mm("messages.jsonl"));
    assert_eq!((r.records_failed, r.records_skipped), (0, 0));
    assert_eq!(c.events.len(), 7);
    assert_eq!(c.metas.len(), 1);
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some(MM_SESSION),
        "decoded from the directory name"
    );

    let user = by_native(
        &c,
        "msg-user-v1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    );
    assert_eq!(
        (user.role, user.event_type),
        (Role::User, EventType::Message)
    );
    assert_eq!(text(user), "list the project files");
    assert!(user.timestamp.utc.is_some());
    assert_eq!(user.metadata["turn_id"], "turn_a");

    let call = by_native(&c, "msg-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
    assert_eq!(call.event_type, EventType::ToolCall);
    assert_eq!(call.model.as_deref(), Some("best"));
    assert_eq!(call.tool_call_ids, vec!["call_001"]);
    assert!(
        matches!(&call.content[0], Part::Reasoning { text, visibility: ReasoningVisibility::Public } if text == "run ls")
    );
    assert!(
        matches!(&call.content[1], Part::ToolCall { id: Some(i), name, arguments } if i == "call_001" && name == "bash" && arguments["command"] == "ls")
    );

    let res = by_native(&c, "msg-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC");
    assert_eq!(
        (res.role, res.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert!(
        matches!(&res.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, output } if i == "call_001" && output[0]["text"] == "README.md\nsrc")
    );

    assert_eq!(
        text(by_native(
            &c,
            "msg-user-v1-EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE"
        )),
        "plain string prompt"
    );
    let comp = by_native(&c, "msg-FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
    assert_eq!(comp.event_type, EventType::Compaction);
    assert_eq!(comp.metadata["tokens_before"], 1234);
    let custom = by_native(&c, "msg-GGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG");
    assert_eq!(custom.role, Role::Other);
    assert_eq!(custom.metadata["custom_type"], "notice");
    assert_eq!(r.tool_calls, 1);
}

#[test]
fn minimax_malformed_records_are_reported_not_fatal() {
    let path = Path::new(FIX).join("minimax/corrupt/.minimax/v2/sessions/2026/10/02/09-00-00-000-session_bXZzXzAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVm/messages.jsonl");
    let (c, r) = parse_file(&MiniMaxParser, &path);
    assert_eq!(c.events.len(), 2);
    assert_eq!(r.records_failed, 4, "{:?}", r.notes);
    for needle in [
        "line 2: invalid JSON",
        "line 3: record is not a JSON object",
        "line 4: message has no role",
        "line 5: record has no message_id",
    ] {
        assert!(
            r.notes.iter().any(|n| n.contains(needle)),
            "{needle} in {:?}",
            r.notes
        );
    }
}

#[test]
fn minimax_snapshot_overlap_collapses_and_provenance_is_kept() {
    let root = Path::new(FIX).join("minimax");
    let out = tmp("mm-import");
    let (events, sources) = import(&root, &out.join("a"));
    // 7 live + 1 snapshot-only message; B repeats identically, C differs (conflict kept).
    assert_eq!(
        events, 9,
        "8 distinct messages + the second variant of the tool result"
    );
    assert_eq!(
        sources.len(),
        3,
        "live file + snapshot + corrupt copy: {sources:?}"
    );
    assert!(sources
        .iter()
        .all(|(_, parser, app)| parser == "minimax" && app == "minimax-code"));
    assert!(
        sources.iter().any(|(p, ..)| {
            Path::new(p).ends_with("messages.jsonl")
                && Path::new(p).starts_with(Path::new(FIX).canonicalize().unwrap())
        }),
        "original path, not a copy"
    );
    // A second run over the same bytes adds nothing.
    let (again, _) = import(&root, &out.join("b"));
    assert_eq!(again, events, "identity is stable across runs");
}

// ----------------------------------------------------------------- deepseek

#[test]
fn deepseek_detects_the_session_log_only() {
    let p = DeepSeekParser;
    let log = probe_file(&ds("session.v4.jsonl")).unwrap();
    let d = p.detect(&log);
    assert!(d.is_hit());
    assert_eq!(d.format, "deepseek-dsh-session-v4");
    assert_eq!(d.confidence, Confidence::Certain);
    let dir = tmp("ds-detect");
    std::fs::write(
        dir.join("session.v4.jsonl"),
        "{\"type\":\"user/message\"}\n",
    )
    .unwrap();
    assert!(
        !p.detect(&probe_file(&dir.join("session.v4.jsonl")).unwrap())
            .is_hit(),
        "needs the header"
    );
    std::fs::copy(ds("session.v4.jsonl"), dir.join("other.jsonl")).unwrap();
    assert!(
        !p.detect(&probe_file(&dir.join("other.jsonl")).unwrap())
            .is_hit(),
        "needs the file name"
    );
    let storage = Path::new("/home/u/.dsh/storages/workspace.json");
    let probe = convolith::source::Probe {
        full_path: storage.display().to_string(),
        ..probe_file(&ds("session.v4.jsonl")).unwrap()
    };
    assert_eq!(
        convolith::parsers::known_unsupported(&probe).unwrap().0,
        "deepseek-dsh-metadata"
    );
}

#[test]
fn deepseek_session_maps_events_and_skips_telemetry() {
    let (c, r) = parse_file(&DeepSeekParser, &ds("session.v4.jsonl"));
    assert_eq!(r.records_failed, 0, "{:?}", r.notes);
    assert_eq!(c.metas.len(), 1);
    let m = &c.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some("5c1f2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f")
    );
    assert_eq!(m.working_directory.as_deref(), Some("/home/demo/proj"));
    assert!(m.started_at.is_some());
    // header + permission/turn/step/request/tool-call telemetry + step/turn end.
    assert_eq!(r.records_skipped, 8, "{:?}", r.notes);
    assert!(r
        .notes
        .iter()
        .any(|n| n.contains("tool/call=1") && n.contains("request/header=1")));

    let user = by_native(&c, "0199aa11-0000-7000-8000-000000000001");
    assert_eq!(
        (user.role, user.event_type),
        (Role::User, EventType::Message)
    );
    assert_eq!(text(user), "fix the bug");

    let a = by_native(&c, "0199aa11-0000-7000-8000-000000000002");
    assert_eq!(
        (a.role, a.event_type),
        (Role::Assistant, EventType::Message)
    );
    assert_eq!(a.model.as_deref(), Some("deepseek-v4"));
    assert!(matches!(&a.content[0], Part::Reasoning { text, .. } if text == "look at the tests"));
    assert!(
        matches!(&a.content[2], Part::ToolCall { id: Some(i), name, arguments } if i == "call_a1" && name == "bash" && arguments["command"] == "cargo test")
    );
    assert_eq!(a.tool_call_ids, vec!["call_a1"]);
    assert_eq!(a.metadata["usage"]["totalTokens"], 15);
    assert!(
        !a.metadata.contains_key("stream"),
        "replay chunks are not retained"
    );

    let res = by_native(&c, "0199aa11-0000-7000-8000-000000000003");
    assert_eq!(
        (res.role, res.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert!(
        matches!(&res.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, .. } if i == "call_a1")
    );
    assert_eq!(
        by_native(&c, "0199aa11-0000-7000-8000-000000000004").event_type,
        EventType::Reasoning
    );

    let title = by_native(&c, "seq-6");
    assert_eq!(
        (title.role, title.event_type),
        (Role::System, EventType::SystemNote)
    );
    assert_eq!(text(title), "Fix the bug");
    // Unknown event types are kept, not dropped.
    let todo = by_native(&c, "seq-11");
    assert!(
        matches!(&todo.content[0], Part::Opaque { kind, raw: Some(_), .. } if kind == "todo/write")
    );
}

#[test]
fn deepseek_malformed_records_are_reported_not_fatal() {
    let path = Path::new(FIX).join("deepseek/corrupt/.dsh/sessions/--p--/session-5c1f2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f/session.v4.jsonl");
    let (c, r) = parse_file(&DeepSeekParser, &path);
    assert_eq!(r.records_failed, 3, "{:?}", r.notes);
    assert_eq!(c.events.len(), 2, "user + assistant survive");
    for needle in [
        "line 3: invalid JSON",
        "line 4: record is not a JSON object",
        "line 5: record has no string `type`",
    ] {
        assert!(
            r.notes.iter().any(|n| n.contains(needle)),
            "{needle} in {:?}",
            r.notes
        );
    }
}

#[test]
fn deepseek_zstd_log_imports_with_original_provenance_and_stable_identity() {
    let store = tmp("ds-zstd");
    let session =
        store.join(".dsh/sessions/--home-demo-proj--/session-5c1f2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f");
    std::fs::create_dir_all(&session).unwrap();
    let plain = std::fs::read(ds("session.v4.jsonl")).unwrap();
    std::fs::write(
        session.join("session.v4.jsonl.zstd"),
        zstd::encode_all(&plain[..], 3).unwrap(),
    )
    .unwrap();

    let out = tmp("ds-out");
    let (events, sources) = import(&store, &out.join("a"));
    // user (its splice collapses onto it) + title + 2 assistant + tool result + todo.
    assert_eq!(events, 6, "{sources:?}");
    assert_eq!(sources.len(), 1);
    let (path, parser, app) = &sources[0];
    assert_eq!(
        (parser.as_str(), app.as_str()),
        ("deepseek", "deepseek-dsh")
    );
    assert!(
        path.ends_with("session.v4.jsonl.zstd!/session.v4.jsonl"),
        "{path}"
    );
    assert!(
        path.starts_with(store.canonicalize().unwrap().to_str().unwrap()),
        "original location kept: {path}"
    );
    let (again, _) = import(&store, &out.join("b"));
    assert_eq!(again, events);
}
