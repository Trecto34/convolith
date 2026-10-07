//! Acceptance tests for the `pi`, `oh_my_pi`, `opencode` and `hermes` parsers,
//! driven through the public library API against the synthetic fixtures in
//! `fixtures/{pi,oh-my-pi,opencode,hermes}`.

use convolith::config::Config;
use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::discover::probe_file;
use convolith::importer::{ImportOptions, Importer};
use convolith::model::{Event, EventDraft, EventType, Part, ReasoningVisibility, Role, RunStats};
use convolith::parser::{ConversationMeta, EventSink, ParseContext, Registry, SourceParser};
use convolith::parsers::{
    claude_code::ClaudeCodeParser, codex::CodexParser, hermes::HermesParser,
    opencode::OpenCodeParser, pi::PiParser, register_all,
};
use convolith::secrets::RedactionHit;
use convolith::source::{Confidence, ParseReport, Source};
use convolith::timeutil::TimestampConfidence;
use std::path::{Path, PathBuf};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

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

fn parse_lines(p: &dyn SourceParser, dir: &str, name: &str) -> (Collect, ParseReport) {
    let path = Path::new(dir).join(name);
    let (sink, report) = parse_with(p, &path, 64 * 1024 * 1024).unwrap();
    // Accounting identity: every non-blank line is exactly one outcome.
    let lines = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count() as u64;
    assert_eq!(
        report.records_examined, lines,
        "{name}: examined must equal non-blank lines"
    );
    assert_eq!(
        sink.events.len() as u64 + report.records_skipped + report.records_failed,
        lines,
        "{name}: events + skipped + failed must equal non-blank lines"
    );
    (sink, report)
}

fn pi(dir: &str, name: &str) -> (Collect, ParseReport) {
    parse_lines(&PiParser::pi(), &format!("{FIX}/{dir}"), name)
}
fn omp(name: &str) -> (Collect, ParseReport) {
    parse_lines(&PiParser::oh_my_pi(), &format!("{FIX}/oh-my-pi"), name)
}
fn hm_lines(name: &str) -> (Collect, ParseReport) {
    parse_lines(&HermesParser, &format!("{FIX}/hermes"), name)
}

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-poh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Build a SQLite database from a `.sql` fixture.
fn build_db(sql: &str, dest: &Path) {
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(dest).unwrap();
    c.execute_batch(&std::fs::read_to_string(format!("{FIX}/{sql}")).unwrap())
        .unwrap();
}

fn text(e: &EventDraft) -> String {
    e.content
        .iter()
        .filter_map(|p| p.as_text())
        .collect::<Vec<_>>()
        .join("|")
}
fn types(c: &Collect) -> Vec<EventType> {
    c.events.iter().map(|e| e.event_type).collect()
}
fn by_native<'a>(c: &'a Collect, id: &str) -> &'a EventDraft {
    c.events
        .iter()
        .find(|e| e.native_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no event {id}"))
}
fn accounting(c: &Collect, r: &ParseReport) {
    assert_eq!(
        c.events.len() as u64 + r.records_skipped + r.records_failed,
        r.records_examined
    );
    assert_eq!(c.events.len() as u64, r.events);
}

const OC_STORE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/opencode/home/.local/share/opencode/storage"
);
const OC_SESSION: &str = "ses_3f7a1c9d2ffeKq81ZxWvBnMcLd";
const OC_M2: &str = "msg_9f3c0a1b2002AbCdEfGhIjKlMn";

fn oc_session() -> PathBuf {
    Path::new(OC_STORE).join(format!("session/proj01/{OC_SESSION}.json"))
}

// -------------------------------------------------------------- detection

#[test]
fn detection_needs_provider_evidence() {
    let (p, o) = (PiParser::pi(), PiParser::oh_my_pi());
    let (oc, hm) = (OpenCodeParser, HermesParser);
    let claude = ClaudeCodeParser;
    let codex = CodexParser;
    let all: Vec<&dyn SourceParser> = vec![&p, &o, &oc, &hm, &claude, &codex];
    let claimed_by = |path: &Path| -> Vec<&'static str> {
        let probe = probe_file(path).unwrap();
        all.iter()
            .filter(|d| d.detect(&probe).is_hit())
            .map(|d| d.id())
            .collect()
    };
    let one = |rel: &str, id: &str| {
        let path = Path::new(FIX).join(rel);
        assert_eq!(claimed_by(&path), vec![id], "{rel}");
    };
    for f in ["simple", "corrupt_line", "missing_timestamp"] {
        one(&format!("pi/{f}.jsonl"), "pi");
    }
    one("oh-my-pi/simple.jsonl", "oh_my_pi");
    one("opencode/export.json", "opencode");
    one("hermes/session_20260901_101500_aa11bb.json", "hermes");
    // A transcript is only Hermes by its session_meta record or its location.
    one("hermes/20260902_080000_cc22dd.jsonl", "hermes");
    assert_eq!(claimed_by(&oc_session()), vec!["opencode"]);
    for entry in walk(Path::new(OC_STORE)) {
        let ids = claimed_by(&entry);
        let is_corrupt_part = entry.to_string_lossy().contains("prt_9f3c0a1b2010");
        assert_eq!(
            ids.len(),
            usize::from(!is_corrupt_part),
            "{entry:?} {ids:?}"
        );
    }
    // Existing providers' fixtures stay out of reach of the new parsers.
    for dir in ["claude-code", "codex", "generic", "chatgpt"] {
        for entry in std::fs::read_dir(format!("{FIX}/{dir}")).unwrap() {
            let ids = claimed_by(&entry.unwrap().path());
            assert!(
                !ids.iter()
                    .any(|i| ["pi", "oh_my_pi", "opencode", "hermes"].contains(i)),
                "{dir}: {ids:?}"
            );
        }
    }

    let dir = scratch("detect");
    // Role/content JSONL is not Hermes without session_meta or a .hermes location.
    let plain = dir.join("chat.jsonl");
    std::fs::write(&plain, "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
    assert!(!hm.detect(&probe_file(&plain).unwrap()).is_hit());
    let located = dir.join(".hermes/sessions/20260902_080000_cc22dd.jsonl");
    std::fs::create_dir_all(located.parent().unwrap()).unwrap();
    std::fs::write(&located, "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
    assert_eq!(
        hm.detect(&probe_file(&located).unwrap()).confidence,
        Confidence::Strong
    );
    // A Pi-looking session header with no cwd, and a bare `type: session`, are not Pi.
    let fake = dir.join("fake.jsonl");
    std::fs::write(
        &fake,
        "{\"type\":\"session\",\"id\":\"x\"}\n{\"type\":\"user\"}\n",
    )
    .unwrap();
    assert!(!p.detect(&probe_file(&fake).unwrap()).is_hit());
    // The store location upgrades Strong to Certain; Oh My Pi is picked by .omp.
    let in_pi = dir.join(".pi/agent/sessions/--x--/s.jsonl");
    std::fs::create_dir_all(in_pi.parent().unwrap()).unwrap();
    std::fs::copy(format!("{FIX}/pi/simple.jsonl"), &in_pi).unwrap();
    assert_eq!(
        p.detect(&probe_file(&in_pi).unwrap()).confidence,
        Confidence::Certain
    );
    let in_omp = dir.join(".omp/agent/sessions/--x--/s.jsonl");
    std::fs::create_dir_all(in_omp.parent().unwrap()).unwrap();
    std::fs::copy(format!("{FIX}/pi/simple.jsonl"), &in_omp).unwrap();
    assert!(!p.detect(&probe_file(&in_omp).unwrap()).is_hit());
    assert_eq!(
        o.detect(&probe_file(&in_omp).unwrap()).confidence,
        Confidence::Certain
    );
    // SQLite databases are recognised by their tables, not their names.
    build_db("opencode/opencode.sql", &dir.join("anything.db"));
    build_db("hermes/state.sql", &dir.join("other.db"));
    assert!(oc
        .detect(&probe_file(&dir.join("anything.db")).unwrap())
        .is_hit());
    assert!(!hm
        .detect(&probe_file(&dir.join("anything.db")).unwrap())
        .is_hit());
    assert!(hm
        .detect(&probe_file(&dir.join("other.db")).unwrap())
        .is_hit());
    assert!(!oc
        .detect(&probe_file(&dir.join("other.db")).unwrap())
        .is_hit());
    let _ = std::fs::remove_dir_all(&dir);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

// --------------------------------------------------------------------- pi

#[test]
fn pi_tree_session_keeps_every_branch_and_structured_parts() {
    let (c, r) = pi("pi", "simple.jsonl");
    assert_eq!((r.records_failed, r.records_skipped), (0, 0));
    assert_eq!(c.events.len(), 11);
    assert_eq!(c.metas.len(), 1);
    let m = &c.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some("0199aa11-2222-7333-8444-555566667777")
    );
    assert_eq!(m.working_directory.as_deref(), Some("/home/demo/pi-proj"));
    assert!(m.started_at.is_some());

    let user = by_native(&c, "a1b2c3d4");
    assert_eq!(
        (user.role, user.event_type),
        (Role::User, EventType::Message)
    );
    assert_eq!(user.parent_native_id, None);
    assert_eq!(user.timestamp.confidence, TimestampConfidence::Exact);

    // Thinking text and the tool call are structured parts of one event.
    let call = by_native(&c, "b2c3d4e5");
    assert_eq!(call.event_type, EventType::ToolCall);
    assert_eq!(call.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(call.tool_call_ids, vec!["toolu_pi_001"]);
    assert!(
        matches!(&call.content[0], Part::Reasoning { text, visibility: ReasoningVisibility::Public } if text == "I should run ls.")
    );
    assert!(
        matches!(&call.content[1], Part::ToolCall { id: Some(i), name, arguments } if i == "toolu_pi_001" && name == "bash" && arguments["command"] == "ls")
    );
    assert_eq!(call.metadata["stop_reason"], "toolUse");

    let res = by_native(&c, "c3d4e5f6");
    assert_eq!(
        (res.role, res.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert!(
        matches!(&res.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, output } if i == "toolu_pi_001" && output[0]["text"] == "README.md\nsrc")
    );
    assert_eq!(res.metadata["tool_name"], "bash");

    // Both children of the first user message survive: nothing is pruned.
    let kids: Vec<_> = c
        .events
        .iter()
        .filter(|e| e.parent_native_id.as_deref() == Some("a1b2c3d4"))
        .map(|e| e.native_id.clone().unwrap())
        .collect();
    assert_eq!(kids, vec!["b2c3d4e5", "e5f6a7b8"]);

    let bash = by_native(&c, "a7b8c9d0");
    assert!(matches!(&bash.content[0], Part::ToolCall { id: None, name, .. } if name == "bash"));
    assert_eq!(bash.metadata["exit_code"], 0);
    assert_eq!(by_native(&c, "b8c9d0e1").event_type, EventType::Compaction);
    assert_eq!(text(by_native(&c, "c9d0e1f2")), "List files");
    // Unknown and runtime-state entries are preserved, not dropped.
    assert!(
        matches!(&by_native(&c, "d0e1f2a3").content[0], Part::Opaque { kind, raw: Some(_), .. } if kind == "future_entry_kind")
    );
    assert!(
        matches!(&by_native(&c, "f6a7b8c9").content[0], Part::Opaque { kind, .. } if kind == "model_change")
    );
    assert_eq!(r.tool_calls, 1);
    accounting(&c, &r);
}

#[test]
fn pi_corrupt_lines_are_reported_not_fatal() {
    let (c, r) = pi("pi", "corrupt_line.jsonl");
    assert_eq!(r.records_failed, 3, "{:?}", r.notes);
    assert_eq!(c.events.len(), 4);
    assert!(r
        .notes
        .iter()
        .any(|n| n.contains("line 3") && n.contains("invalid JSON")));
    assert!(r.notes.iter().any(|n| n.contains("not a JSON object")));
}

#[test]
fn pi_missing_timestamp_is_never_fabricated() {
    let (c, _) = pi("pi", "missing_timestamp.jsonl");
    let stamped = c
        .events
        .iter()
        .filter(|e| e.timestamp.utc.is_some())
        .count();
    assert_eq!(stamped, 1, "only the header carries a timestamp");
    for e in c.events.iter().skip(1) {
        assert_eq!(e.timestamp.confidence, TimestampConfidence::Unknown);
        assert!(e.timestamp.original.is_none());
    }
}

#[test]
fn oh_my_pi_title_slot_names_the_conversation() {
    let (c, r) = omp("simple.jsonl");
    assert_eq!(c.metas[0].title.as_deref(), Some("Fix the build"));
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("0199bb22-3333-7444-8555-666677778888")
    );
    assert_eq!(r.records_skipped, 1);
    assert_eq!(c.events.len(), 4);
    assert!(
        matches!(&by_native(&c, "33cc44dd").content[0], Part::Opaque { kind, .. } if kind == "mode_change")
    );
    assert_eq!(PiParser::oh_my_pi().application(), "oh-my-pi");
    assert_ne!(PiParser::pi().id(), PiParser::oh_my_pi().id());
}

// --------------------------------------------------------------- opencode

#[test]
fn opencode_file_store_session() {
    let (c, r) = parse_with(&OpenCodeParser, &oc_session(), 1 << 20).unwrap();
    let m = &c.metas[0];
    assert_eq!(m.native_id.as_deref(), Some(OC_SESSION));
    assert_eq!(m.title.as_deref(), Some("Add a hello script"));
    assert_eq!(m.working_directory.as_deref(), Some("/home/demo/oc-proj"));
    // 3 messages + 2 finished tool results; step-start/finish skipped; 1 corrupt part.
    assert_eq!(c.events.len(), 5, "{:?}", r.notes);
    assert_eq!((r.records_skipped, r.records_failed), (2, 1));
    assert!(r.notes.iter().any(|n| n.contains("prt_9f3c0a1b2010")));
    accounting(&c, &r);

    let user = by_native(&c, "msg_9f3c0a1b2001AbCdEfGhIjKlMn");
    assert_eq!(
        (user.role, user.model.as_deref()),
        (Role::User, Some("claude-sonnet-4-5"))
    );
    assert_eq!(
        user.timestamp.confidence,
        TimestampConfidence::DatabaseDerived
    );

    let a = by_native(&c, OC_M2);
    assert_eq!(a.event_type, EventType::ToolCall);
    assert_eq!(
        a.parent_native_id.as_deref(),
        Some("msg_9f3c0a1b2001AbCdEfGhIjKlMn")
    );
    assert_eq!(
        a.tool_call_ids,
        vec!["toolu_oc_001", "toolu_oc_002", "toolu_oc_003"]
    );
    assert!(
        matches!(&a.content[0], Part::Reasoning { text, .. } if text == "Need a shell script.")
    );
    assert!(
        matches!(&a.content[1], Part::ToolCall { name, arguments, .. } if name == "write" && arguments["filePath"] == "hello.sh")
    );
    // A tool that never finished is recorded as such, with no invented result.
    assert_eq!(a.metadata["unfinished_tool_calls"][0], "toolu_oc_003");

    let ok = c
        .events
        .iter()
        .find(|e| {
            e.native_id
                .as_deref()
                .is_some_and(|n| n.ends_with("2004ZyXwVuTsRqPoNm:result"))
        })
        .unwrap();
    assert_eq!(
        (ok.role, ok.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert_eq!(ok.parent_native_id.as_deref(), Some(OC_M2));
    assert!(
        matches!(&ok.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, output } if i == "toolu_oc_001" && output == "Wrote file successfully.")
    );
    let bad = c
        .events
        .iter()
        .find(|e| {
            e.native_id
                .as_deref()
                .is_some_and(|n| n.ends_with("2005ZyXwVuTsRqPoNm:result"))
        })
        .unwrap();
    assert!(
        matches!(&bad.content[0], Part::ToolResult { is_error: true, output, .. } if output == "permission denied")
    );

    let last = by_native(&c, "msg_9f3c0a1b2003AbCdEfGhIjKlMn");
    assert_eq!(last.event_type, EventType::Message);
    assert!(matches!(&last.content[1], Part::Opaque { kind, .. } if kind == "patch"));
    assert_eq!(r.tool_calls, 1);
}

#[test]
fn opencode_message_and_part_files_are_covered_by_their_session() {
    let msg = Path::new(OC_STORE).join(format!(
        "message/{OC_SESSION}/msg_9f3c0a1b2001AbCdEfGhIjKlMn.json"
    ));
    let part = Path::new(OC_STORE)
        .join("part/msg_9f3c0a1b2001AbCdEfGhIjKlMn/prt_9f3c0a1b2001ZyXwVuTsRqPoNm.json");
    for p in [msg, part] {
        let (c, r) = parse_with(&OpenCodeParser, &p, 1 << 20).unwrap();
        assert!(c.metas.is_empty() && c.events.is_empty());
        assert_eq!((r.records_examined, r.records_failed), (0, 0));
        assert!(r.notes[0].contains("session file"));
    }
}

#[test]
fn opencode_export_bundle() {
    let (c, r) = parse_with(
        &OpenCodeParser,
        &Path::new(FIX).join("opencode/export.json"),
        1 << 20,
    )
    .unwrap();
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("ses_7b2e4d6f8ffeLm93AaSsDdFfGg")
    );
    assert_eq!(c.events.len(), 2);
    assert_eq!(r.records_failed, 1);
    assert_eq!(text(&c.events[1]), "hi back");
    accounting(&c, &r);
}

#[test]
fn opencode_sqlite_store_streams_one_conversation_per_session() {
    let dir = scratch("ocdb");
    let db = dir.join("opencode.db");
    build_db("opencode/opencode.sql", &db);
    let (c, r) = parse_with(&OpenCodeParser, &db, 1 << 20).unwrap();
    assert_eq!(c.metas.len(), 2);
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("ses_5c1d3e5f7ffeNn24BbVvCcXxZz")
    );
    assert_eq!(
        c.metas[1].metadata["parent_session_id"],
        "ses_5c1d3e5f7ffeNn24BbVvCcXxZz"
    );
    // main: user, assistant (+tool result); sub: user. One unparsable message row, one bad part.
    assert_eq!(c.events.len(), 4, "{:?}", r.notes);
    assert_eq!(r.records_failed, 2, "{:?}", r.notes);
    assert!(r.notes.iter().any(|n| n.contains("sqlite:")));
    let a = by_native(&c, "msg_5c1d3e5f7002AaSsDdFfGgHhJj");
    assert_eq!(a.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(a.timestamp.confidence, TimestampConfidence::DatabaseDerived);
    assert!(a.content.iter().any(|p| matches!(p, Part::ToolCall { id: Some(i), name, .. } if i == "call_db_1" && name == "read")));
    assert_eq!(r.conversations, 2);
    accounting(&c, &r);
    // The source database is only read.
    let before = std::fs::read(&db).unwrap();
    let _ = parse_with(&OpenCodeParser, &db, 1 << 20).unwrap();
    assert_eq!(before, std::fs::read(&db).unwrap());
    // A SQLite file that is not OpenCode's is an error for that source only.
    let other = dir.join("other.db");
    build_db("hermes/state.sql", &other);
    assert!(parse_with(&OpenCodeParser, &other, 1 << 20).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------------- hermes

#[test]
fn hermes_sqlite_sessions_and_messages() {
    let dir = scratch("hmdb");
    let db = dir.join("state.db");
    build_db("hermes/state.sql", &db);
    let (c, r) = parse_with(&HermesParser, &db, 1 << 20).unwrap();
    assert_eq!(c.metas.len(), 2);
    let m = &c.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some("hermes-session-20260830_233734_3ff33e")
    );
    assert_eq!(m.title.as_deref(), Some("List dir"));
    assert_eq!(
        m.working_directory.as_deref(),
        Some("/home/demo/hermes-proj")
    );
    assert_eq!(m.metadata["source"], "cli");
    assert_eq!(c.metas[1].metadata["source"], "telegram");
    // session 1: system prompt + 6 rows; session 2: 1 row.
    assert_eq!(c.events.len(), 8);
    assert_eq!((r.records_failed, r.records_skipped), (0, 0));
    assert_eq!(c.events[0].event_type, EventType::SystemNote);
    assert_eq!(text(&c.events[0]), "You are Hermes.");

    let call = &c.events[2];
    assert_eq!(call.event_type, EventType::ToolCall);
    assert!(matches!(&call.content[0], Part::Reasoning { text, .. } if text == "Run ls first."));
    assert!(
        matches!(&call.content[1], Part::ToolCall { id: Some(i), name, arguments } if i == "call_hermes_1" && name == "terminal" && arguments["command"] == "ls")
    );
    assert_eq!(call.metadata["finish_reason"], "tool_calls");
    assert_eq!(
        call.timestamp.confidence,
        TimestampConfidence::ProviderDerived
    );
    let res = &c.events[3];
    assert_eq!(
        (res.role, res.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert!(
        matches!(&res.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, .. } if i == "call_hermes_1")
    );
    // History Hermes no longer replays is kept, flagged.
    let old = &c.events[5];
    assert_eq!(old.metadata["inactive"], true);
    assert_eq!(old.metadata["compacted"], true);
    assert_eq!(c.events[6].event_type, EventType::Compaction);
    assert_eq!(r.conversations, 2);
    accounting(&c, &r);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hermes_json_session_log() {
    let path = Path::new(FIX).join("hermes/session_20260901_101500_aa11bb.json");
    let (c, r) = parse_with(&HermesParser, &path, 1 << 20).unwrap();
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("hermes-session-20260901_101500_aa11bb")
    );
    assert_eq!(c.metas[0].model.as_deref(), Some("claude-sonnet-4-5"));
    // Naive local ISO text is not an instant: kept literally, never assumed UTC.
    assert!(c.metas[0].started_at.is_none());
    assert_eq!(
        c.metas[0].metadata["session_start_original"],
        "2026-09-01T10:15:00.123456"
    );
    assert!(c.metas[0].ended_at.is_some());
    // system prompt + 4 messages; the non-object entry fails alone.
    assert_eq!(c.events.len(), 5);
    assert_eq!(r.records_failed, 1, "{:?}", r.notes);
    assert!(
        matches!(&c.events[2].content[0], Part::ToolCall { name, arguments, .. } if name == "calculator" && arguments["expr"] == "2+2")
    );
    assert!(matches!(&c.events[3].content[0], Part::ToolResult { output, .. } if output == "4"));
    accounting(&c, &r);
}

#[test]
fn hermes_jsonl_transcript() {
    let (c, r) = hm_lines("20260902_080000_cc22dd.jsonl");
    assert_eq!(c.metas[0].model.as_deref(), Some("gpt-5"));
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("hermes-session-20260902_080000_cc22dd")
    );
    assert_eq!(c.events.len(), 2);
    assert_eq!((r.records_failed, r.records_skipped), (1, 1));
    assert_eq!(types(&c), vec![EventType::Message; 2]);
    assert_eq!(
        c.events[0].timestamp.confidence,
        TimestampConfidence::Unknown
    );
    assert_eq!(
        c.events[0].timestamp.original.as_deref(),
        Some("2026-09-02T08:00:01.000000")
    );
    assert_eq!(c.events[1].timestamp.confidence, TimestampConfidence::Exact);
}

// ----------------------------------------------------------- shared limits

#[test]
fn oversize_records_and_empty_sources_are_not_fatal() {
    let dir = scratch("limits");
    let pi_parser = PiParser::pi();
    let cases: Vec<(&dyn SourceParser, PathBuf)> = vec![
        (&pi_parser, Path::new(FIX).join("pi/simple.jsonl")),
        (
            &HermesParser,
            Path::new(FIX).join("hermes/20260902_080000_cc22dd.jsonl"),
        ),
        (
            &HermesParser,
            Path::new(FIX).join("hermes/session_20260901_101500_aa11bb.json"),
        ),
        (&OpenCodeParser, oc_session()),
        (&OpenCodeParser, Path::new(FIX).join("opencode/export.json")),
    ];
    for (parser, path) in cases {
        let (c, r) = parse_with(parser, &path, 100).unwrap();
        assert!(
            r.records_failed > 0,
            "{path:?} should hit the 100-byte limit"
        );
        accounting(&c, &r);
    }
    let empty = dir.join("empty.jsonl");
    std::fs::write(&empty, "\n\n").unwrap();
    let (c, r) = parse_with(&PiParser::pi(), &empty, 1 << 20).unwrap();
    assert!(c.metas.is_empty() && c.events.is_empty() && r.records_examined == 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- end to end

fn run_import(input: &Path, out: &Path) -> RunStats {
    let mut reg = Registry::new(vec![]);
    register_all(&mut reg);
    let opts = ImportOptions {
        output: out.to_path_buf(),
        ..Default::default()
    };
    Importer::new(opts, Config::default(), reg)
        .unwrap()
        .import(&[input.to_path_buf()])
        .unwrap()
}

fn stored_events(out: &Path) -> Vec<Event> {
    let layout = Layout {
        root: out.to_path_buf(),
    };
    let mut shards: Vec<_> = std::fs::read_dir(layout.events_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    shards.sort();
    let mut events = Vec::new();
    for s in shards {
        read_jsonl_zst::<Event>(
            &s,
            1 << 28,
            |e| {
                events.push(e);
                Ok(())
            },
            &mut |_, m| panic!("{m}"),
        )
        .unwrap();
    }
    events
}

fn copy_tree(from: &Path, to: &Path) {
    for f in walk(from) {
        let dest = to.join(f.strip_prefix(from).unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(&f, dest).unwrap();
    }
}

#[test]
fn end_to_end_import_is_labelled_and_reimport_is_idempotent() {
    let base = scratch("e2e");
    let home = base.join("home");
    let put = |from: &str, to: &str| {
        let dest = home.join(to);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(format!("{FIX}/{from}"), dest).unwrap();
    };
    put("pi/simple.jsonl", ".pi/agent/sessions/--home-demo-pi-proj--/2026-05-01T10-00-00-000Z_0199aa11-2222-7333-8444-555566667777.jsonl");
    put("oh-my-pi/simple.jsonl", ".omp/agent/sessions/--home-demo-omp-proj--/2026-05-02T09-00-00-000Z_0199bb22-3333-7444-8555-666677778888.jsonl");
    copy_tree(
        &Path::new(FIX).join("opencode/home/.local/share/opencode/storage"),
        &home.join(".local/share/opencode/storage"),
    );
    put(
        "opencode/export.json",
        ".local/share/opencode/export/ses_7b2e4d6f8ffeLm93AaSsDdFfGg.json",
    );
    build_db(
        "opencode/opencode.sql",
        &home.join(".local/share/opencode/opencode.db"),
    );
    build_db("hermes/state.sql", &home.join(".hermes/state.db"));
    put(
        "hermes/session_20260901_101500_aa11bb.json",
        ".hermes/sessions/session_20260901_101500_aa11bb.json",
    );
    put(
        "hermes/20260902_080000_cc22dd.jsonl",
        ".hermes/sessions/20260902_080000_cc22dd.jsonl",
    );
    // Decoy no parser may claim.
    std::fs::write(
        home.join("notes.jsonl"),
        "{\"type\":\"note\",\"text\":\"x\"}\n",
    )
    .unwrap();

    let out = base.join("out");
    let first = run_import(&home, &out);
    assert!(first.accounting_holds(), "{first:?}");
    // pi + omp + 13 opencode store files (corrupt part is unclaimed) + export + db + 3 hermes.
    assert_eq!(first.supported_sources, 20, "{first:?}");
    assert_eq!(first.unsupported_sources, 2, "{first:?}");
    // 11 + 4 + 5 + 2 + 4 + 8 + 5 + 2 events; failures: 1 corrupt part, 1 bad bundle entry,
    // 2 bad rows in opencode.db, 1 bad JSON-log entry, 1 broken transcript line.
    assert_eq!(first.events_total, 41, "{first:?}");
    assert_eq!(first.records_failed, 6, "{first:?}");
    assert_eq!(first.conflicts, 0);
    assert_eq!(first.events_duplicate, 0, "{first:?}");

    let events = stored_events(&out);
    assert_eq!(events.len(), 41);
    let label = |native: &str| {
        let e = events
            .iter()
            .find(|e| e.metadata.get("native_id").and_then(|v| v.as_str()) == Some(native))
            .unwrap_or_else(|| panic!("{native}"));
        (e.provider.clone(), e.application.clone())
    };
    assert_eq!(label("a1b2c3d4"), ("pi".into(), "pi".into()));
    assert_eq!(label("33cc44dd"), ("pi".into(), "oh-my-pi".into()));
    assert_eq!(label(OC_M2), ("opencode".into(), "opencode".into()));
    assert_eq!(label("msg_5c1d3e5f7002AaSsDdFfGgHhJj").1, "opencode");
    let tool = events
        .iter()
        .find(|e| e.event_type == EventType::ToolResult && e.application == "hermes-agent")
        .unwrap();
    assert_eq!(tool.provider, "nousresearch");
    let user = events
        .iter()
        .find(|e| e.metadata.get("native_id").and_then(|v| v.as_str()) == Some("a1b2c3d4"))
        .unwrap();
    assert_eq!(
        user.working_directory.as_deref(),
        Some("/home/demo/pi-proj")
    );
    assert_eq!(
        user.session_id.as_deref(),
        Some("0199aa11-2222-7333-8444-555566667777")
    );

    let second = run_import(&home, &out);
    assert!(second.accounting_holds(), "{second:?}");
    assert_eq!(second.events_new, 0, "{second:?}");
    assert_eq!(second.events_duplicate, first.events_total);
    assert_eq!(second.conflicts, 0);
    assert_eq!(stored_events(&out).len(), events.len());
    let _ = std::fs::remove_dir_all(&base);
}
