//! Acceptance tests for the `claude_code` and `codex` parsers, driven through
//! the public library API against the synthetic fixtures in `fixtures/`.

use convolith::config::Config;
use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::discover::probe_file;
use convolith::importer::{ImportOptions, Importer};
use convolith::ledger::Ledger;
use convolith::model::{Event, EventDraft, EventType, Part, ReasoningVisibility, Role, RunStats};
use convolith::parser::{ConversationMeta, EventSink, ParseContext, Registry, SourceParser};
use convolith::parsers::{claude_code::ClaudeCodeParser, codex::CodexParser, register_all};
use convolith::secrets::RedactionHit;
use convolith::source::{Confidence, ParseReport, Source};
use convolith::timeutil::TimestampConfidence;
use std::path::{Path, PathBuf};

const CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/claude-code");
const CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/codex");

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

fn parse(p: &dyn SourceParser, dir: &str, name: &str) -> (Collect, ParseReport) {
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

fn cc(name: &str) -> (Collect, ParseReport) {
    parse(&ClaudeCodeParser, CLAUDE, name)
}

#[test]
fn claude_large_queue_and_bridge_preambles_do_not_hide_history() {
    use serde_json::json;
    let t = scratch("large-preambles");
    let store = t.join(".claude/projects/demo");
    std::fs::create_dir_all(&store).unwrap();
    let fixture = std::fs::read_to_string(format!("{CLAUDE}/simple.jsonl")).unwrap();
    let (baseline, baseline_report) = cc("simple.jsonl");
    let session = "11111111-1111-4111-8111-111111111111";
    // Match the real writer's envelope-before-content order.
    let queue = |n| {
        format!(
            r#"{{"type":"queue-operation","operation":"enqueue","timestamp":"2026-04-01T00:00:00Z","sessionId":"{session}","content":"{}"}}"#,
            "q".repeat(n)
        )
    };
    for (name, prefix) in [
        ("queue", format!("{}\n", queue(32 * 1024))),
        (
            "bridge",
            format!(
                "{}\n{}\n",
                json!({"type":"bridge-session","sessionId":session,"bridgeSessionId":"22222222-2222-4222-8222-222222222222","lastSequenceNum":0}),
                queue(32 * 1024)
            ),
        ),
    ] {
        let path = store.join(format!("{name}.jsonl"));
        std::fs::write(&path, format!("{prefix}{fixture}")).unwrap();
        let probe = probe_file(&path).unwrap();
        assert_eq!(probe.head_bytes.len(), convolith::discover::PROBE_BYTES);
        assert_eq!(
            ClaudeCodeParser.detect(&probe).confidence,
            Confidence::Certain
        );
        let (parsed, report) = parse_with(&ClaudeCodeParser, &path, 64 * 1024 * 1024).unwrap();
        assert_eq!(parsed.events.len(), baseline.events.len());
        assert_eq!(report.records_failed, 0);
        assert_eq!(
            report.records_skipped,
            baseline_report.records_skipped + prefix.lines().count() as u64
        );
    }
    // The larger retry stays bounded, and store naming does not replace schema evidence.
    let path = store.join("over-bound.jsonl");
    std::fs::write(&path, format!("{}\n{fixture}", queue(1024 * 1024 + 100))).unwrap();
    assert!(!ClaudeCodeParser
        .detect(&probe_file(&path).unwrap())
        .is_hit());
    let path = t.join("unrelated.jsonl");
    std::fs::write(&path, format!("{}\n{fixture}", queue(32 * 1024))).unwrap();
    assert!(!ClaudeCodeParser
        .detect(&probe_file(&path).unwrap())
        .is_hit());
}

#[test]
fn bookkeeping_and_installation_assets_are_classified_without_parsing_history() {
    let t = scratch("classification");
    let path = t.join(".claude/projects/demo/bridge.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = r#"{"type":"bridge-session","sessionId":"11111111-1111-4111-8111-111111111111","bridgeSessionId":"22222222-2222-4222-8222-222222222222","lastSequenceNum":0}"#;
    std::fs::write(&path, body).unwrap();
    let probe = probe_file(&path).unwrap();
    assert!(!ClaudeCodeParser.detect(&probe).is_hit());
    assert_eq!(
        convolith::parsers::known_unsupported(&probe).unwrap().0,
        "claude-bookkeeping"
    );
    std::fs::write(&path, format!("{body}\ninvalid\n")).unwrap();
    assert!(convolith::parsers::known_unsupported(&probe_file(&path).unwrap()).is_none());
    let asset = t.join("AppData/Local/AnthropicClaude/app-1.0/resources/runtime.js");
    std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
    std::fs::write(&asset, "synthetic runtime asset").unwrap();
    let probe = probe_file(&asset).unwrap();
    assert!(convolith::parsers::registry().best(&probe).is_none());
    assert_eq!(
        convolith::parsers::known_unsupported(&probe).unwrap().0,
        "desktop-installation-file"
    );
}

#[test]
fn recovering_an_unsupported_source_updates_classification_without_changing_source_id() {
    let t = scratch("recover-classification");
    let input = t.join(".claude/projects/demo/session.jsonl");
    std::fs::create_dir_all(input.parent().unwrap()).unwrap();
    std::fs::copy(format!("{CLAUDE}/simple.jsonl"), &input).unwrap();
    let out = t.join("archive");
    let opts = ImportOptions {
        output: out.clone(),
        machine_id: Some("windows".into()),
        ..Default::default()
    };
    let mut importer =
        Importer::new(opts.clone(), Default::default(), Registry::new(vec![])).unwrap();
    importer.import(std::slice::from_ref(&input)).unwrap();
    drop(importer);
    let row = || {
        let db = rusqlite::Connection::open(out.join("provenance/provenance.sqlite")).unwrap();
        db.query_row(
            "select source_id,parser,format,machine_id from source",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
    };
    let before = row();
    assert_eq!(before.1, None);
    assert_eq!(before.2, "unknown");
    let opts = ImportOptions {
        machine_id: Some("windows/new-stable-id".into()),
        ..opts
    };
    let mut importer =
        Importer::new(opts, Default::default(), convolith::parsers::registry()).unwrap();
    importer.import(std::slice::from_ref(&input)).unwrap();
    drop(importer);
    let after = row();
    assert_eq!(after.0, before.0);
    assert_eq!(after.1.as_deref(), Some("claude_code"));
    assert_eq!(after.2, "claude-code-jsonl");
    assert_eq!(
        after.3, "windows",
        "historical machine attribution is preserved"
    );
    assert_eq!(
        convolith::report::stats(&out).unwrap().sources_unsupported,
        0
    );
}
fn cx(name: &str) -> (Collect, ParseReport) {
    parse(&CodexParser, CODEX, name)
}

fn types(c: &Collect) -> Vec<EventType> {
    c.events.iter().map(|e| e.event_type).collect()
}
fn text(e: &EventDraft) -> String {
    e.content
        .iter()
        .filter_map(|p| p.as_text())
        .collect::<Vec<_>>()
        .join("|")
}

// -------------------------------------------------------------- detection

#[test]
fn detection_needs_provider_evidence() {
    let claude = ClaudeCodeParser;
    let codex = CodexParser;
    for entry in std::fs::read_dir(CLAUDE).unwrap() {
        let probe = probe_file(&entry.unwrap().path()).unwrap();
        assert_eq!(
            claude.detect(&probe).confidence,
            Confidence::Strong,
            "{}",
            probe.full_path
        );
        assert!(!codex.detect(&probe).is_hit(), "{}", probe.full_path);
    }
    for entry in std::fs::read_dir(CODEX).unwrap() {
        let probe = probe_file(&entry.unwrap().path()).unwrap();
        assert_eq!(
            codex.detect(&probe).confidence,
            Confidence::Strong,
            "{}",
            probe.full_path
        );
        assert!(!claude.detect(&probe).is_hit(), "{}", probe.full_path);
    }

    let dir = scratch("detect");
    let write = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        probe_file(&p).unwrap()
    };
    // A .jsonl extension, chat-shaped records or a provider-named directory are
    // not evidence by themselves.
    for body in [
        "{\"a\":1}\n{\"b\":2}\n",
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        "{\"type\":\"message\",\"role\":\"user\",\"content\":\"hi\"}\n",
        "not json at all\n",
        "",
    ] {
        for name in [
            "x.jsonl",
            "home/.claude/projects/p/x.jsonl",
            "home/.codex/sessions/rollout-x.jsonl",
        ] {
            let probe = write(name, body);
            assert!(
                !claude.detect(&probe).is_hit(),
                "claude claimed {name}: {body}"
            );
            assert!(
                !codex.detect(&probe).is_hit(),
                "codex claimed {name}: {body}"
            );
        }
    }
    // Right bytes, wrong extension.
    let probe = write(
        "session.json",
        &std::fs::read_to_string(format!("{CLAUDE}/simple.jsonl")).unwrap(),
    );
    assert!(!claude.detect(&probe).is_hit());

    // Provider directory plus schema evidence is the strongest claim.
    let probe = write(
        "home/.claude/projects/-home-demo/s.jsonl",
        &std::fs::read_to_string(format!("{CLAUDE}/simple.jsonl")).unwrap(),
    );
    assert_eq!(claude.detect(&probe).confidence, Confidence::Certain);
    let probe = write(
        "home/.codex/sessions/2026/04/01/rollout-2026-04-01T10-00-00-x.jsonl",
        &std::fs::read_to_string(format!("{CODEX}/simple.jsonl")).unwrap(),
    );
    assert_eq!(codex.detect(&probe).confidence, Confidence::Certain);

    // A first record larger than the probe window is still recognised.
    let big = "x".repeat(40_000);
    let claude_big = format!(
        "{{\"parentUuid\":null,\"isSidechain\":false,\"sessionId\":\"s\",\"type\":\"user\",\"uuid\":\"u\",\"message\":{{\"role\":\"user\",\"content\":\"{big}\"}}}}\n"
    );
    assert!(claude.detect(&write("big/c.jsonl", &claude_big)).is_hit());
    let codex_big = format!(
        "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"i\",\"base_instructions\":{{\"text\":\"{big}\"}}}}}}\n"
    );
    assert!(codex.detect(&write("big/x.jsonl", &codex_big)).is_hit());

    // Registry picks the right parser for each.
    let mut reg = Registry::new(vec![]);
    register_all(&mut reg);
    let best = |p: &str| reg.best(&probe_file(Path::new(p)).unwrap()).unwrap().parser;
    assert_eq!(best(&format!("{CLAUDE}/tool_calls.jsonl")), "claude_code");
    assert_eq!(best(&format!("{CODEX}/tool_calls.jsonl")), "codex");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------ claude code

#[test]
fn claude_simple_conversation() {
    let (c, r) = cc("simple.jsonl");
    assert_eq!((r.records_failed, r.records_skipped), (0, 2));
    assert!(r
        .notes
        .iter()
        .any(|n| n.contains("file-history-snapshot=1") && n.contains("last-prompt=1")));
    assert_eq!(c.metas.len(), 1);
    let m = &c.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
    assert_eq!(m.native_session_id, m.native_id);
    assert_eq!(m.working_directory.as_deref(), Some("/home/demo/proj"));
    assert_eq!(m.branch.as_deref(), Some("main"));
    assert!(m.model.is_none(), "user events must not inherit a model");
    assert_eq!(
        types(&c),
        [
            EventType::SystemNote,
            EventType::Message,
            EventType::Message,
            EventType::SystemNote
        ]
    );
    let (summary, user, asst, title) = (&c.events[0], &c.events[1], &c.events[2], &c.events[3]);
    assert_eq!(text(summary), "Greeting demo");
    assert_eq!(summary.timestamp.confidence, TimestampConfidence::Unknown);
    assert_eq!(user.role, Role::User);
    assert_eq!(
        user.native_id.as_deref(),
        Some("c0000001-0000-4000-8000-000000000001")
    );
    assert!(user.parent_native_id.is_none());
    assert_eq!(text(user), "Hello Claude");
    assert_eq!(asst.role, Role::Assistant);
    assert_eq!(asst.parent_native_id, user.native_id);
    assert_eq!(asst.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(
        asst.timestamp.rfc3339().as_deref(),
        Some("2026-03-01T10:00:02.500000000Z")
    );
    assert_eq!(asst.timestamp.confidence, TimestampConfidence::Exact);
    assert_eq!(asst.metadata["message_id"], "msg_s1a");
    assert_eq!(asst.metadata["usage"]["output_tokens"], 7);
    assert_eq!(asst.metadata["source_line"], 4);
    assert_eq!(text(title), "Greeting");
}

#[test]
fn claude_tool_calls_results_thinking_and_compaction() {
    use EventType::*;
    let (c, r) = cc("tool_calls.jsonl");
    assert_eq!(
        (r.records_failed, r.records_skipped, r.tool_calls),
        (0, 0, 2)
    );
    assert_eq!(
        types(&c),
        [
            Message, Reasoning, ToolCall, ToolResult, ToolCall, ToolResult, Message, Message,
            Compaction, Attachment
        ]
    );
    let e = &c.events;
    // Thinking is emitted only because its text is literally present...
    assert_eq!(
        e[1].content,
        [Part::Reasoning {
            text: "Need to list the directory.".into(),
            visibility: ReasoningVisibility::Public
        }]
    );
    // ...and a signature-only block yields no invented reasoning text.
    assert!(matches!(&e[2].content[0], Part::Opaque { kind, raw: None, .. } if kind == "thinking"));
    let id1 = "toolu_01AAAAAAAAAAAAAAAAAAAAAA";
    assert_eq!(
        e[2].content[1],
        Part::ToolCall {
            id: Some(id1.into()),
            name: "Bash".into(),
            arguments: serde_json::json!({"command": "ls"})
        }
    );
    assert_eq!(e[2].tool_call_ids, [id1]);
    assert_eq!(e[2].role, Role::Assistant);
    assert_eq!(
        e[3].content,
        [Part::ToolResult {
            tool_call_id: Some(id1.into()),
            output: "a.txt\nb.txt".into(),
            is_error: false
        }]
    );
    assert_eq!(
        (e[3].role, e[3].metadata["source_role"].as_str()),
        (Role::Tool, Some("user"))
    );
    assert_eq!(
        e[3].metadata["source_tool_assistant_uuid"],
        "c0000013-0000-4000-8000-000000000013"
    );
    match &e[5].content[0] {
        Part::ToolResult {
            is_error, output, ..
        } => {
            assert!(is_error);
            assert_eq!(output[0]["text"], "File not found");
        }
        p => panic!("{p:?}"),
    }
    // Text + image in one user record stays one message, image kept verbatim.
    assert_eq!(e[7].role, Role::User);
    assert!(
        matches!(&e[7].content[1], Part::Opaque { kind, raw: Some(raw), .. } if kind == "image" && raw["source"]["data"] == "iVBORw0KGgo=")
    );
    assert_eq!(e[8].role, Role::System);
    assert_eq!(e[8].metadata["compact_metadata"]["preTokens"], 1234);
    assert_eq!(
        e[8].metadata["logical_parent_uuid"],
        "c0000018-0000-4000-8000-000000000018"
    );
    assert!(e[8].parent_native_id.is_none());
    assert!(
        matches!(&e[9].content[0], Part::Opaque { kind, .. } if kind == "attachment:hook_success")
    );
}

#[test]
fn claude_subagent_sidechain_is_its_own_conversation() {
    let (main, _) = cc("subagent_main.jsonl");
    let (side, _) = cc("subagent_agent.jsonl");
    let parent = "33333333-3333-4333-8333-333333333333";
    assert_eq!(main.metas[0].native_id.as_deref(), Some(parent));
    assert!(matches!(&main.events[1].content[0], Part::ToolCall { name, .. } if name == "Task"));
    let m = &side.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some(&*format!("{parent}/agent-a1b2c3d4e5f60718"))
    );
    assert_ne!(
        m.native_id, main.metas[0].native_id,
        "must not merge into the parent"
    );
    assert_eq!(m.metadata["parent_session_id"], parent);
    assert_eq!(m.metadata["is_sidechain"], true);
    for e in &side.events {
        assert_eq!(e.agent.as_deref(), Some("a1b2c3d4e5f60718"));
        assert_eq!(e.metadata["is_sidechain"], true);
        assert_eq!(e.metadata["agent_id"], "a1b2c3d4e5f60718");
    }
    assert!(main.events.iter().all(|e| e.agent.is_none()));
}

#[test]
fn claude_fork_and_retry_keep_the_uuid_tree() {
    let (a, _) = cc("fork_retry.jsonl");
    let (b, _) = cc("fork_resumed.jsonl");
    // A retry is two children of the same parent; both survive, in file order.
    assert_eq!(a.events[1].parent_native_id, a.events[2].parent_native_id);
    assert_eq!(
        (text(&a.events[1]).as_str(), text(&a.events[2]).as_str()),
        ("First attempt", "Retry attempt")
    );
    assert_eq!(a.events[3].parent_native_id, a.events[2].native_id);
    // The forked file reuses native ids for the shared prefix.
    let ids = |c: &Collect| {
        c.events
            .iter()
            .map(|e| e.native_id.clone().unwrap())
            .collect::<Vec<_>>()
    };
    let shared: Vec<_> = ids(&b)
        .into_iter()
        .filter(|i| ids(&a).contains(i))
        .collect();
    assert_eq!(shared.len(), 3);
    assert_ne!(a.metas[0].native_id, b.metas[0].native_id);
}

#[test]
fn claude_corrupt_lines_are_reported_not_fatal() {
    let (c, r) = cc("corrupt_line.jsonl");
    assert_eq!(c.events.len(), 2);
    assert_eq!(r.records_failed, 4);
    let notes = r.notes.join("\n");
    for needle in [
        "line 2: invalid JSON",
        "line 4: record is not a JSON object",
        "line 5: record has no string `type`",
        "line 6: user record has no message object",
    ] {
        assert!(notes.contains(needle), "missing {needle:?} in {notes}");
    }
    assert_eq!(
        (text(&c.events[0]).as_str(), text(&c.events[1]).as_str()),
        ("before the damage", "after the damage")
    );
}

#[test]
fn claude_missing_timestamp_is_never_fabricated() {
    let (c, _) = cc("missing_timestamp.jsonl");
    let t = |i: usize| &c.events[i].timestamp;
    assert!(t(0).utc.is_none() && t(0).original.is_none());
    assert_eq!(t(0).confidence, TimestampConfidence::Unknown);
    assert!(t(1).utc.is_none());
    assert_eq!(t(1).original.as_deref(), Some("yesterday"));
    assert_eq!(t(1).confidence, TimestampConfidence::Unknown);
    assert_eq!(t(2).confidence, TimestampConfidence::Exact);
}

// ------------------------------------------------------------------ codex

#[test]
fn codex_simple_rollout() {
    let (c, r) = cx("simple.jsonl");
    assert_eq!(r.records_failed, 0);
    assert_eq!(r.records_skipped, 5);
    let notes = r.notes.join("\n");
    assert!(
        notes.contains("event_msg:user_message (echo of response_item)=1"),
        "{notes}"
    );
    assert!(
        notes.contains("token_usage_record=1") && notes.contains("turn_context=1"),
        "{notes}"
    );
    let m = &c.metas[0];
    assert_eq!(
        m.native_id.as_deref(),
        Some("019a0001-aaaa-7bbb-8ccc-000000000001")
    );
    assert_eq!(m.working_directory.as_deref(), Some("/home/demo/proj"));
    assert_eq!(m.branch.as_deref(), Some("main"));
    assert_eq!(
        m.git_remote.as_deref(),
        Some("https://example.invalid/demo.git")
    );
    use EventType::*;
    assert_eq!(types(&c), [SystemNote, Message, Reasoning, Message]);
    let e = &c.events;
    assert_eq!(text(&e[0]), "You are a coding agent.");
    assert_eq!(e[0].role, Role::System);
    assert_eq!(e[1].role, Role::User);
    assert!(
        e[1].native_id.is_none(),
        "no id in the source, none invented"
    );
    assert!(e[1].model.is_none());
    assert_eq!(
        e[2].content[0],
        Part::Reasoning {
            text: "Greeting the user".into(),
            visibility: ReasoningVisibility::Summary
        }
    );
    assert!(
        matches!(&e[2].content[1], Part::Opaque { kind, raw: Some(raw), .. } if kind == "encrypted_reasoning" && raw["encrypted_content"] == "gAAAAABfake-encrypted-blob")
    );
    assert_eq!(
        e[2].model.as_deref(),
        Some("gpt-5-codex"),
        "model comes from turn_context"
    );
    assert_eq!(e[3].role, Role::Assistant);
    assert_eq!(e[3].model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(
        e[3].native_id.as_deref(),
        Some("msg_019a0001-0000-7000-8000-000000000002")
    );
    assert_eq!(
        e[3].timestamp.rfc3339().as_deref(),
        Some("2026-04-01T10:00:04Z")
    );
    assert_eq!(e[3].metadata["ordinal"], 4);
}

#[test]
fn codex_tool_calls_and_outputs() {
    use EventType::*;
    let (c, r) = cx("tool_calls.jsonl");
    assert_eq!((r.records_failed, r.tool_calls), (0, 4));
    assert_eq!(
        types(&c),
        [
            SystemNote, Message, ToolCall, ToolResult, ToolCall, ToolResult, ToolCall, ToolResult,
            Opaque, ToolCall, Message
        ]
    );
    let e = &c.events;
    let c1 = "call_AbC123xYz456QwErTy789012";
    assert_eq!(
        e[2].content,
        [Part::ToolCall {
            id: Some(c1.into()),
            name: "shell".into(),
            arguments: serde_json::json!({"command": ["ls"]})
        }]
    );
    assert_eq!(e[2].native_id.as_deref(), Some(&*format!("{c1}:call")));
    assert_eq!(e[3].native_id.as_deref(), Some(&*format!("{c1}:output")));
    assert_eq!(e[3].role, Role::Tool);
    assert!(
        matches!(&e[3].content[0], Part::ToolResult { tool_call_id: Some(i), is_error: false, .. } if i == c1)
    );
    assert!(
        matches!(&e[4].content[0], Part::ToolCall { name, arguments, .. } if name == "apply_patch" && arguments.as_str().unwrap().starts_with("*** Begin Patch"))
    );
    assert!(
        matches!(&e[6].content[0], Part::ToolCall { name, arguments, .. } if name == "local_shell" && arguments["command"][0] == "echo")
    );
    assert!(
        matches!(&e[7].content[0], Part::ToolResult { is_error: true, .. }),
        "success:false is an error result"
    );
    assert!(
        matches!(&e[8].content[0], Part::Opaque { kind, raw: Some(raw), .. } if kind == "item_completed:CommandExecution" && raw["exit_code"] == 1)
    );
    assert!(matches!(&e[9].content[0], Part::ToolCall { name, .. } if name == "web_search"));
}

#[test]
fn codex_subagent_fork_and_interruption() {
    let (s, _) = cx("subagent.jsonl");
    assert_eq!(s.metas[0].metadata["is_subagent"], true);
    assert_eq!(s.events[0].metadata["is_subagent"], true);
    assert_eq!(s.events[1].metadata["recipient"], "/root/architect");
    assert!(
        matches!(&s.events[1].content[1], Part::Opaque { kind, .. } if kind == "encrypted_content")
    );
    assert!(
        matches!(&s.events[2].content[0], Part::Opaque { kind, .. } if kind == "item_completed:SubAgentActivity")
    );

    let (a, _) = cx("fork_retry.jsonl");
    assert_eq!(
        types(&a),
        [
            EventType::SystemNote,
            EventType::Message,
            EventType::Message,
            EventType::Interruption,
            EventType::Message
        ]
    );
    assert_eq!(text(&a.events[3]), "interrupted");
    let (b, _) = cx("fork_resumed.jsonl");
    assert_eq!(
        b.metas[0].metadata["forked_from_id"],
        "019a0004-aaaa-7bbb-8ccc-000000000004"
    );
    let ids = |c: &Collect| {
        c.events
            .iter()
            .filter_map(|e| e.native_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&b).iter().filter(|i| ids(&a).contains(i)).count(), 2);
}

#[test]
fn codex_corrupt_lines_are_reported_not_fatal() {
    let (c, r) = cx("corrupt_line.jsonl");
    assert_eq!((c.events.len(), r.records_failed), (3, 3));
    let notes = r.notes.join("\n");
    for needle in [
        "line 2: invalid JSON",
        "line 4: record is not a JSON object",
        "line 5: response_item has no string `type`",
    ] {
        assert!(notes.contains(needle), "missing {needle:?} in {notes}");
    }
    assert_eq!(text(&c.events[2]), "after the damage");
}

#[test]
fn codex_missing_timestamp_is_never_fabricated() {
    let (c, _) = cx("missing_timestamp.jsonl");
    assert_eq!(c.events.len(), 4);
    assert!(c.events[0].timestamp.utc.is_none() && c.events[1].timestamp.utc.is_none());
    assert_eq!(
        c.events[1].timestamp.confidence,
        TimestampConfidence::Unknown
    );
    assert_eq!(
        c.events[2].timestamp.original.as_deref(),
        Some("not-a-time")
    );
    assert!(c.events[2].timestamp.utc.is_none());
    assert_eq!(c.events[3].timestamp.confidence, TimestampConfidence::Exact);
}

#[test]
fn codex_legacy_bare_items() {
    let (c, r) = cx("legacy.jsonl");
    assert_eq!(r.records_skipped, 1);
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("019a0008-aaaa-7bbb-8ccc-000000000008")
    );
    use EventType::*;
    assert_eq!(
        types(&c),
        [SystemNote, Message, ToolCall, ToolResult, Message]
    );
    assert_eq!(text(&c.events[0]), "Legacy instructions.");
    assert!(c.events.iter().skip(1).all(|e| e.timestamp.utc.is_none()));
    assert_eq!(
        c.events[3].native_id.as_deref(),
        Some("call_legacy0000000000001:output")
    );
}

#[test]
fn codex_filename_id_when_header_missing() {
    let dir = scratch("fname");
    let p = dir.join("rollout-2026-04-01T10-00-00-019a0009-aaaa-7bbb-8ccc-000000000009.jsonl");
    std::fs::write(&p, "{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}}\n").unwrap();
    let (c, _) = parse_with(&CodexParser, &p, 1 << 20).unwrap();
    assert_eq!(
        c.metas[0].native_id.as_deref(),
        Some("019a0009-aaaa-7bbb-8ccc-000000000009")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------- shared limits

#[test]
fn oversize_records_and_empty_sources_are_not_fatal() {
    let dir = scratch("limits");
    for (parser, fixtures) in [
        (&ClaudeCodeParser as &dyn SourceParser, CLAUDE),
        (&CodexParser, CODEX),
    ] {
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let path = entry.unwrap().path();
            // A tiny record limit turns most lines into failures, never into an error.
            let (c, r) = parse_with(parser, &path, 100).unwrap();
            let lines = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count() as u64;
            assert_eq!(
                c.events.len() as u64 + r.records_skipped + r.records_failed,
                lines,
                "{path:?}"
            );
            assert!(
                r.records_failed > 0,
                "{path:?} should hit the 100-byte limit"
            );
        }
        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "\n\n").unwrap();
        let (c, r) = parse_with(parser, &empty, 1 << 20).unwrap();
        assert!(c.metas.is_empty() && c.events.is_empty());
        assert_eq!(r.records_examined, 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- end to end

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-pcc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

#[test]
fn end_to_end_import_keeps_provenance_and_reimport_is_idempotent() {
    let base = scratch("e2e");
    let input = base.join("home");
    let claude_dir = input.join(".claude/projects/-home-demo-proj");
    let codex_dir = input.join(".codex/sessions/2026/04/01");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_dir).unwrap();
    let mut expected_events = 0usize;
    let mut expected_failed = 0u64;
    let mut expected_calls = 0u64;
    for (parser, fixtures, dest, prefix) in [
        (
            &ClaudeCodeParser as &dyn SourceParser,
            CLAUDE,
            &claude_dir,
            "",
        ),
        (
            &CodexParser,
            CODEX,
            &codex_dir,
            "rollout-2026-04-01T10-00-00-",
        ),
    ] {
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let src = entry.unwrap().path();
            let (c, r) = parse_with(parser, &src, 64 << 20).unwrap();
            expected_events += c.events.len();
            expected_failed += r.records_failed;
            expected_calls += r.tool_calls;
            std::fs::copy(
                &src,
                dest.join(format!(
                    "{prefix}{}",
                    src.file_name().unwrap().to_str().unwrap()
                )),
            )
            .unwrap();
        }
    }
    // Decoys no parser may claim.
    std::fs::write(
        input.join("notes.jsonl"),
        "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
    )
    .unwrap();

    let out = base.join("out");
    let first = run_import(&input, &out);
    assert!(first.accounting_holds(), "{first:?}");
    assert_eq!(first.supported_sources, 16, "{first:?}");
    assert_eq!(first.unsupported_sources, 1, "{first:?}");
    assert_eq!(first.records_failed, expected_failed);
    assert_eq!(first.parse_errors, expected_failed);
    assert_eq!(first.events_total as usize, expected_events);
    // The forked files re-observe the events they copied: 3 Claude uuids, 2 Codex message ids.
    assert_eq!(first.events_duplicate, 5, "{first:?}");
    assert_eq!(first.events_new as usize, expected_events - 5);
    assert_eq!(first.conflicts, 0);
    assert_eq!(first.tool_calls, expected_calls);

    let events = stored_events(&out);
    assert_eq!(events.len(), first.events_new as usize);

    // Provenance: the Claude user message points back at the exact source line
    // of the exact file bytes, under the native identity tier.
    let ev = events
        .iter()
        .find(|e| {
            e.metadata.get("native_id").and_then(|v| v.as_str())
                == Some("c0000011-0000-4000-8000-000000000011")
        })
        .expect("claude event");
    assert_eq!(
        (ev.provider.as_str(), ev.application.as_str()),
        ("anthropic", "claude-code")
    );
    assert_eq!(ev.role, Role::User);
    assert_eq!(ev.working_directory.as_deref(), Some("/home/demo/proj"));
    assert_eq!(ev.branch.as_deref(), Some("main"));
    assert_eq!(
        ev.session_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222")
    );
    assert_eq!(ev.metadata["source_line"], 1);
    let ledger = Ledger::open(&Layout { root: out.clone() }.provenance_db()).unwrap();
    let prov = ledger.provenance_for(&ev.event_id).unwrap();
    assert_eq!(prov.len(), 1);
    let p = &prov[0];
    let file = claude_dir.join("tool_calls.jsonl");
    assert!(
        p.source_path
            .replace('\\', "/")
            .ends_with(".claude/projects/-home-demo-proj/tool_calls.jsonl"),
        "{}",
        p.source_path
    );
    assert_eq!(p.parser, "claude_code");
    assert_eq!(
        p.record_id.as_deref(),
        Some("c0000011-0000-4000-8000-000000000011")
    );
    assert_eq!(p.record_index, Some(0));
    assert_eq!(p.identity_tier, "native");
    assert_eq!(
        p.source_sha256.as_deref(),
        Some(convolith::id::sha256_file(&file).unwrap().as_str())
    );
    assert_eq!(p.source_status.as_deref(), Some("parsed"));

    // A shared (forked) event is observed from both files.
    let shared = events
        .iter()
        .find(|e| {
            e.metadata.get("native_id").and_then(|v| v.as_str())
                == Some("c0000031-0000-4000-8000-000000000031")
        })
        .unwrap();
    let paths: Vec<String> = ledger
        .provenance_for(&shared.event_id)
        .unwrap()
        .into_iter()
        .map(|p| p.source_path)
        .collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths.iter().any(|p| p.ends_with("fork_retry.jsonl"))
            && paths.iter().any(|p| p.ends_with("fork_resumed.jsonl"))
    );

    // Codex: provider labels, tool-call linkage and native tier.
    let call = events
        .iter()
        .find(|e| {
            e.metadata.get("native_id").and_then(|v| v.as_str())
                == Some("call_AbC123xYz456QwErTy789012:call")
        })
        .expect("codex call");
    assert_eq!(
        (call.provider.as_str(), call.application.as_str()),
        ("openai", "codex")
    );
    assert_eq!(call.event_type, EventType::ToolCall);
    let cprov = ledger.provenance_for(&call.event_id).unwrap();
    assert_eq!(
        (cprov[0].parser.as_str(), cprov[0].identity_tier.as_str()),
        ("codex", "native")
    );
    assert!(cprov[0]
        .source_path
        .contains("rollout-2026-04-01T10-00-00-tool_calls.jsonl"));
    drop(ledger);

    // Idempotent re-import: nothing new, everything a duplicate, no new shard records.
    let second = run_import(&input, &out);
    assert!(second.accounting_holds(), "{second:?}");
    assert_eq!(second.events_new, 0, "{second:?}");
    assert_eq!(second.events_duplicate, first.events_total);
    assert_eq!(second.conflicts, 0);
    assert_eq!(stored_events(&out).len(), events.len());
    let _ = std::fs::remove_dir_all(&base);
}
