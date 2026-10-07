//! Acceptance tests for the `gemini_cli` and `antigravity` parsers, driven
//! through the public library API against the synthetic fixtures in
//! `fixtures/{gemini-cli,antigravity}`. The fixtures mirror the structure of the
//! real local stores; none of their text comes from a real conversation.

use convolith::config::Config;
use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::discover::probe_file;
use convolith::importer::{ImportOptions, Importer};
use convolith::model::{Event, EventDraft, EventType, Part, ReasoningVisibility, Role, RunStats};
use convolith::parser::{ConversationMeta, EventSink, ParseContext, Registry, SourceParser};
use convolith::parsers::{
    antigravity::AntigravityParser, gemini_cli::GeminiCliParser, register_all,
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

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-gem-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

const GC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/gemini-cli/home/.gemini/tmp/demo-project"
);
const AG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/antigravity/home/.gemini/antigravity-cli"
);
const AG_CONV: &str = "5e1f0c2a-7b3d-4a9e-8c10-2d4f6a8b0c12";
const SESSION: &str = "0b7c1d2e-3f40-4a51-8b62-73c84d95e6f1";

fn gc(name: &str) -> PathBuf {
    Path::new(GC).join("chats").join(name)
}
fn ag() -> PathBuf {
    Path::new(AG).join(format!(
        "brain/{AG_CONV}/.system_generated/logs/transcript_full.jsonl"
    ))
}
fn session() -> PathBuf {
    gc("session-2026-02-03T10-00-0b7c1d2e.jsonl")
}
fn run_gc(p: &Path) -> (Collect, ParseReport) {
    parse_with(&GeminiCliParser, p, 1 << 20).unwrap()
}
fn by_native<'a>(c: &'a Collect, id: &str) -> &'a EventDraft {
    c.events
        .iter()
        .find(|e| e.native_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no event {id}"))
}
fn msg(n: u32) -> String {
    format!("11111111-aaaa-4bbb-8ccc-{n:012}")
}
fn accounting(c: &Collect, r: &ParseReport) {
    assert_eq!(
        c.events.len() as u64 + r.records_skipped + r.records_failed,
        r.records_examined
    );
    assert_eq!(c.events.len() as u64, r.events);
}

// -------------------------------------------------------------- detection

#[test]
fn detection_needs_the_store_shape_not_the_name() {
    let g = GeminiCliParser;
    let a = AntigravityParser;
    for f in [
        "session-2026-02-03T10-00-0b7c1d2e.jsonl",
        "session-2026-01-01T09-00-legacy01.json",
    ] {
        let d = g.detect(&probe_file(&gc(f)).unwrap());
        assert_eq!(d.confidence, Confidence::Certain, "{f}");
        assert!(!a.detect(&probe_file(&gc(f)).unwrap()).is_hit());
    }
    let d = a.detect(&probe_file(&ag()).unwrap());
    assert_eq!(d.confidence, Confidence::Certain);
    assert_eq!(d.format, "antigravity-transcript-jsonl");
    assert!(!g.detect(&probe_file(&ag()).unwrap()).is_hit());
    // Derived copies and unrelated files are not claimed.
    let trunc = ag().with_file_name("transcript.jsonl");
    assert!(!a.detect(&probe_file(&trunc).unwrap()).is_hit());
    let dir = scratch("detect");
    std::fs::write(dir.join("session-x.jsonl"), "{\"type\":\"note\"}\n").unwrap();
    std::fs::write(
        dir.join("transcript_full.jsonl"),
        "{\"role\":\"user\",\"content\":\"x\"}\n",
    )
    .unwrap();
    for f in ["session-x.jsonl", "transcript_full.jsonl"] {
        let p = probe_file(&dir.join(f)).unwrap();
        assert!(!g.detect(&p).is_hit() && !a.detect(&p).is_hit(), "{f}");
    }
    // A first step longer than the probe window is still recognised.
    let big = format!(
        "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-03-04T12:00:00Z\",\"content\":\"{}\"}}\n",
        "x".repeat(40_000)
    );
    std::fs::write(dir.join("transcript_full.jsonl"), big).unwrap();
    assert!(a
        .detect(&probe_file(&dir.join("transcript_full.jsonl")).unwrap())
        .is_hit());
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- gemini cli

#[test]
fn gemini_cli_session_maps_roles_thoughts_tools_and_usage() {
    let (c, r) = run_gc(&session());
    assert_eq!(c.metas.len(), 1);
    let m = &c.metas[0];
    assert_eq!(m.native_id.as_deref(), Some(SESSION));
    assert_eq!(
        m.working_directory.as_deref(),
        Some("/home/demo/demo-project")
    );
    assert!(m.started_at.is_some() && m.ended_at.is_some());
    assert_eq!(m.metadata["kind"], "main");

    let user = by_native(&c, &msg(1));
    assert_eq!(
        (user.role, user.event_type),
        (Role::User, EventType::Message)
    );
    assert_eq!(user.content, vec![Part::text("list the files")]);
    assert_eq!(user.timestamp.confidence, TimestampConfidence::Exact);

    // The rewritten message wins: one assistant event, with its tool result.
    assert_eq!(
        c.events
            .iter()
            .filter(|e| e.native_id.as_deref() == Some(&msg(2)))
            .count(),
        1
    );
    let call = by_native(&c, &msg(2));
    assert_eq!(
        (call.role, call.event_type),
        (Role::Assistant, EventType::ToolCall)
    );
    assert_eq!(call.model.as_deref(), Some("gemini-3-flash-preview"));
    assert_eq!(call.metadata["usage"]["total"], 115);
    assert!(
        matches!(&call.content[0], Part::Reasoning { text, visibility: ReasoningVisibility::Summary } if text == "Planning\n\nRun ls in the project.")
    );
    assert!(
        matches!(&call.content[1], Part::ToolCall { id: Some(i), name, arguments } if i == "run_shell_command_1" && name == "run_shell_command" && arguments["command"] == "ls")
    );
    assert_eq!(call.tool_call_ids, vec!["run_shell_command_1"]);

    let res = by_native(&c, &format!("{}/run_shell_command_1", msg(2)));
    assert_eq!(
        (res.role, res.event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert_eq!(res.parent_native_id.as_deref(), Some(msg(2).as_str()));
    assert!(
        matches!(&res.content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, output } if i == "run_shell_command_1" && output == "README.md\nsrc")
    );
    assert_eq!(res.metadata["status"], "success");
    assert_eq!(res.metadata["display_name"], "Shell");
    assert_eq!(res.timestamp.confidence, TimestampConfidence::Exact);

    let answer = by_native(&c, &msg(3));
    assert_eq!(answer.event_type, EventType::Message);
    assert_eq!(
        answer.content,
        vec![Part::text("There are two entries: README.md and src.")]
    );
    // Unknown fields are kept, not dropped.
    assert_eq!(answer.metadata["unmapped"]["futureField"]["x"], 1);

    assert_eq!(by_native(&c, &msg(4)).event_type, EventType::SystemNote);
    let failed = by_native(&c, &format!("{}/run_shell_command_2", msg(6)));
    assert!(
        matches!(&failed.content[0], Part::ToolResult { is_error: true, output, .. } if output == "[Operation Cancelled] Reason: User cancelled")
    );
    assert_eq!(failed.metadata["status"], "cancelled");
    let err = by_native(&c, &msg(7));
    assert_eq!((err.role, err.event_type), (Role::System, EventType::Error));
    assert!(
        matches!(&by_native(&c, &msg(8)).content[0], Part::Opaque { kind, raw: Some(_), .. } if kind == "somethingnew")
    );

    // $set patches and the superseded first copy of the rewritten message.
    assert_eq!(
        (r.records_failed, r.records_skipped),
        (0, 3),
        "{:?}",
        r.notes
    );
    assert_eq!(r.tool_calls, 2);
    accounting(&c, &r);
}

#[test]
fn gemini_cli_legacy_json_document() {
    let (c, r) = run_gc(&gc("session-2026-01-01T09-00-legacy01.json"));
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("7d2e9a40-1b3c-4d5e-8f60-a1b2c3d4e5f6")
    );
    assert_eq!(c.events.len(), 2);
    assert_eq!(c.events[0].content, vec![Part::text("hello")]);
    assert_eq!(c.events[1].model.as_deref(), Some("gemini-2.5-pro"));
    assert_eq!(c.events[1].metadata["usage"]["total"], 8);
    assert_eq!(r.records_examined, 2);
    // Not a session document at all: an error for this source only.
    let dir = scratch("legacy");
    std::fs::write(dir.join("session-bad.json"), "[1,2]").unwrap();
    assert!(parse_with(&GeminiCliParser, &dir.join("session-bad.json"), 1 << 20).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_cli_malformed_lines_are_reported_not_fatal() {
    let (c, r) = run_gc(&gc("session-2026-02-03T11-00-corrupt01.jsonl"));
    assert_eq!(r.records_failed, 3, "{:?}", r.notes);
    assert_eq!(c.events.len(), 2);
    assert!(r
        .notes
        .iter()
        .any(|n| n.contains("line 3") && n.contains("invalid JSON")));
    assert!(r.notes.iter().any(|n| n.contains("not a JSON object")));
    assert!(r.notes.iter().any(|n| n.contains("no string `type`")));
    accounting(&c, &r);
}

// ------------------------------------------------------------ antigravity

#[test]
fn antigravity_transcript_maps_steps_and_keeps_unknowns() {
    let (c, r) = parse_with(&AntigravityParser, &ag(), 1 << 20).unwrap();
    assert_eq!(c.metas.len(), 1);
    assert_eq!(c.metas[0].native_id.as_deref(), Some(AG_CONV));
    let e = &c.events;
    assert_eq!(
        (e[0].role, e[0].event_type),
        (Role::User, EventType::Message)
    );
    assert_eq!(e[0].content, vec![Part::text("show the readme")]);
    assert_eq!(e[0].metadata["step_index"], 0);
    // A step with no content is kept as opaque, never dropped.
    assert_eq!(
        (e[1].role, e[1].event_type),
        (Role::System, EventType::SystemNote)
    );
    assert!(
        matches!(&e[1].content[0], Part::Opaque { kind, .. } if kind == "CONVERSATION_HISTORY")
    );
    assert_eq!(e[2].event_type, EventType::ToolCall);
    assert!(matches!(
        &e[2].content[0],
        Part::Reasoning {
            visibility: ReasoningVisibility::Public,
            ..
        }
    ));
    assert!(
        matches!(&e[2].content[1], Part::ToolCall { id: None, name, arguments } if name == "view_file" && arguments["AbsolutePath"] == "/home/demo/README.md")
    );
    // Tool output has no call id in the source: none is invented.
    assert_eq!(
        (e[3].role, e[3].event_type),
        (Role::Tool, EventType::ToolResult)
    );
    assert!(
        matches!(&e[3].content[0], Part::ToolResult { tool_call_id: None, is_error: false, output } if output == "# Demo\nhello")
    );
    assert_eq!(e[3].metadata["step_type"], "VIEW_FILE");
    assert_eq!(e[4].event_type, EventType::Message);
    assert_eq!(e[4].content.len(), 2);
    assert!(matches!(
        &e[5].content[0],
        Part::ToolResult { is_error: true, .. }
    ));
    assert_eq!(e[5].metadata["exit_code"], 2);
    assert!(
        matches!(&e[6].content[1], Part::Image { source_ref: Some(u), mime: Some(m), .. } if u == "/home/demo/shot.png" && m == "image/png")
    );
    assert_eq!(
        (e[7].role, e[7].event_type),
        (Role::System, EventType::Error)
    );
    assert_eq!(e[7].metadata["error_code"], 503);
    assert_eq!(e[8].metadata["unmapped"]["novel_field"], true);
    assert_eq!(e[9].event_type, EventType::Opaque);
    // Duplicate step_index is metadata, not identity.
    assert!(e.iter().all(|d| d.native_id.is_none()));
    assert_eq!(e[8].metadata["step_index"], e[9].metadata["step_index"]);
    assert_eq!(e[0].timestamp.confidence, TimestampConfidence::Exact);
    assert_eq!(r.records_failed, 1, "{:?}", r.notes);
    assert_eq!(r.tool_calls, 1);
    accounting(&c, &r);
}

// ------------------------------------------- identity, provenance, inventory

fn tree(tag: &str) -> (PathBuf, PathBuf) {
    let base = scratch(tag);
    let home = base.join("home");
    copy_tree(&Path::new(FIX).join("gemini-cli/home"), &home);
    copy_tree(&Path::new(FIX).join("antigravity/home"), &home);
    // A stand-in for the protobuf SQLite trajectory store (`*.db` is not kept in git).
    let db = home.join(format!(
        ".gemini/antigravity-cli/conversations/{AG_CONV}.db"
    ));
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(db, b"SQLite format 3\0synthetic").unwrap();
    (base, home)
}

#[test]
fn import_is_idempotent_keeps_provenance_and_explains_unsupported_files() {
    let (base, home) = tree("e2e");
    let out = base.join("out");
    let first: RunStats = run_import(&home, &out);
    assert!(first.accounting_holds(), "{first:?}");
    // 3 chat files + 1 transcript parsed; transcript.jsonl, the db and .project_root are not.
    assert_eq!(first.supported_sources, 4, "{first:?}");
    assert_eq!(first.unsupported_sources, 3, "{first:?}");
    // session 10 (the rewritten message folded to one) + corrupt 2 + legacy 2 + transcript 10.
    assert_eq!(first.events_new, 24, "{first:?}");
    assert_eq!(first.events_duplicate, 0);
    assert_eq!(first.conflicts, 0);

    let events = stored_events(&out);
    let user = events
        .iter()
        .find(|e| e.metadata.get("native_id").and_then(|v| v.as_str()) == Some(&msg(1)))
        .unwrap();
    assert_eq!(
        (user.provider.as_str(), user.application.as_str()),
        ("google", "gemini-cli")
    );
    assert_eq!(
        user.working_directory.as_deref(),
        Some("/home/demo/demo-project")
    );
    let prov = convolith::report::provenance(&out, &user.event_id).unwrap();
    let p = &prov[0];
    assert!(
        p.source_path
            .replace('\\', "/")
            .ends_with(".gemini/tmp/demo-project/chats/session-2026-02-03T10-00-0b7c1d2e.jsonl"),
        "{}",
        p.source_path
    );
    assert_eq!(p.parser, "gemini_cli");
    assert_eq!(p.identity_tier, "native");
    let tr = events
        .iter()
        .find(|e| e.application == "antigravity-cli")
        .unwrap();
    let tp = convolith::report::provenance(&out, &tr.event_id).unwrap();
    assert!(tp[0].source_path.replace('\\', "/").ends_with(&format!(
        "brain/{AG_CONV}/.system_generated/logs/transcript_full.jsonl"
    )));
    assert_eq!(tp[0].identity_tier, "fingerprint");

    // Unsupported files carry their reason in the inventory.
    let inv: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("reports/SOURCE_INVENTORY.json")).unwrap())
            .unwrap();
    let unsupported: Vec<(String, String)> = inv["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["status"] == "unsupported")
        .map(|s| {
            (
                s["format"].as_str().unwrap().to_string(),
                s["note"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let note = |fmt: &str| {
        unsupported
            .iter()
            .find(|(f, _)| f == fmt)
            .unwrap_or_else(|| panic!("{fmt}: {unsupported:?}"))
            .1
            .clone()
    };
    assert!(note("antigravity-sqlite-protobuf").contains("protobuf"));
    assert!(note("antigravity-transcript-jsonl").contains("transcript_full.jsonl"));
    assert!(unsupported
        .iter()
        .all(|(_, n)| n.ends_with("left unresolved")));

    // Same bytes in a second dataset get the same event ids (identity is stable).
    let out2 = base.join("out2");
    run_import(&home, &out2);
    let ids = |evs: &[Event]| {
        let mut v: Vec<String> = evs.iter().map(|e| e.event_id.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(ids(&events), ids(&stored_events(&out2)));
    // Re-importing into the same dataset adds nothing.
    let again = run_import(&home, &out);
    assert_eq!(
        (again.events_new, again.events_duplicate),
        (0, 24),
        "{again:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn registered_in_the_builtin_registry() {
    let mut reg = Registry::new(vec![]);
    register_all(&mut reg);
    for id in ["gemini_cli", "antigravity"] {
        assert!(reg.by_id(id).is_some(), "{id}");
    }
}
