//! Acceptance tests (spec §65–§71) driven through the real `convolith` binary,
//! with synthetic fixtures generated here.

use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::model::{Event, Part, Role};
use convolith::report;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-acc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Run the CLI; returns (exit code, stdout + stderr).
fn cli(args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_convolith"))
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

fn import(input: &Path, out: &Path, extra: &[&str]) -> (i32, String) {
    let mut a = vec![
        "import",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    a.extend_from_slice(extra);
    cli(&a)
}

fn events(out: &Path) -> Vec<Event> {
    let mut shards: Vec<PathBuf> = walkdir::WalkDir::new(Layout { root: out.into() }.events_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.zst"))
        .map(|e| e.into_path())
        .collect();
    shards.sort();
    let mut all = Vec::new();
    for s in shards {
        read_jsonl_zst::<Event>(
            &s,
            1 << 28,
            |e| {
                all.push(e);
                Ok(())
            },
            &mut |_, m| panic!("{m}"),
        )
        .unwrap();
    }
    all
}

/// Event uuids are unique per session (a shared uuid is, by design, one event).
fn uuid(session: &str, n: u64) -> String {
    format!("{}-000{}-4000-8000-{n:012}", &session[..8], &session[35..])
}

const SESSION: &str = "5e55105e-0000-4000-8000-000000000001";

/// A Claude Code session file holding messages `range` (uuid = counter).
fn claude_session(
    session: &str,
    cwd: &str,
    branch: &str,
    range: std::ops::RangeInclusive<u64>,
) -> String {
    let mut s = String::new();
    for n in range {
        let (ty, role) = if n % 2 == 1 {
            ("user", "user")
        } else {
            ("assistant", "assistant")
        };
        let parent = if n == 1 {
            Value::Null
        } else {
            json!(uuid(session, n - 1))
        };
        let content = if role == "user" {
            json!(format!("question number {n}"))
        } else {
            json!([{"type": "text", "text": format!("answer number {n}")}])
        };
        s.push_str(
            &json!({
                "parentUuid": parent, "isSidechain": false, "cwd": cwd, "sessionId": session,
                "gitBranch": branch, "uuid": uuid(session, n),
                "timestamp": format!("2026-03-01T10:{:02}:{:02}.000Z", n / 60, n % 60),
                "type": ty, "message": {"role": role, "content": content}
            })
            .to_string(),
        );
        s.push('\n');
    }
    s
}

fn write(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A Codex rollout with a git remote in its header.
fn codex_rollout(id: &str, cwd: &str, remote: &str, branch: &str) -> String {
    let lines = [
        json!({"timestamp":"2026-04-01T10:00:00.000Z","ordinal":0,"type":"session_meta","payload":{
            "id": id, "timestamp":"2026-04-01T10:00:00.000Z","cwd":cwd,"originator":"codex_cli_rs",
            "cli_version":"0.1.0","source":"cli","base_instructions":{"text":"be helpful"},
            "git":{"commit_hash":"0123456789abcdef0123456789abcdef01234567","branch":branch,"repository_url":remote}}}),
        json!({"timestamp":"2026-04-01T10:00:02.000Z","ordinal":1,"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":format!("hello from {cwd}")}]}}),
        json!({"timestamp":"2026-04-01T10:00:03.000Z","ordinal":2,"type":"response_item","payload":{
            "type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}}),
    ];
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn provenance_counts(out: &Path) -> BTreeMap<String, usize> {
    events(out)
        .into_iter()
        .map(|e| {
            let n = report::provenance(out, &e.event_id).unwrap().len();
            (e.event_id, n)
        })
        .collect()
}

#[test]
fn s65_overlapping_backups_collapse_and_keep_both_provenances() {
    let t = tmp("s65");
    let a = t.join("in/backup-old/.claude/projects/p/s.jsonl");
    let b = t.join("in/backup-new/.claude/projects/p/s.jsonl");
    write(&a, claude_session(SESSION, "/home/u/p", "main", 1..=100));
    write(&b, claude_session(SESSION, "/home/u/p", "main", 1..=150));
    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &[]);
    assert_eq!(code, 0, "{log}");

    let evs = events(&out);
    assert_eq!(evs.len(), 150, "{log}");
    let prov = provenance_counts(&out);
    let mut by_seq: Vec<&Event> = evs.iter().collect();
    by_seq.sort_by_key(|e| e.seq);
    for (i, e) in by_seq.iter().enumerate() {
        let want = if i < 100 { 2 } else { 1 };
        assert_eq!(prov[&e.event_id], want, "event #{i} {}", e.event_id);
    }
    let (vc, vlog) = cli(&["validate", out.to_str().unwrap()]);
    assert_eq!(vc, 0, "{vlog}");
}

#[test]
fn s66_export_and_local_store_are_not_collapsed_without_confident_identity() {
    let t = tmp("s66");
    // ChatGPT export with three messages ...
    write(
        &t.join("in/export/conversations.json"),
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/chatgpt/conversations.json"
        ))
        .unwrap(),
    );
    // ... and a local store with identical texts but no stable ids.
    write(
        &t.join("in/local/chat.jsonl"),
        "{\"role\":\"user\",\"content\":\"question\"}\n{\"role\":\"assistant\",\"content\":\"first answer\"}\n{\"role\":\"assistant\",\"content\":\"second answer\"}\n",
    );
    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &[]);
    assert_eq!(code, 0, "{log}");
    let evs = events(&out);
    let export = evs.iter().filter(|e| e.provider == "openai").count();
    let local = evs.len() - export;
    assert_eq!(export, 3, "{log}");
    assert_eq!(
        local, 3,
        "same text, no confident identity: must stay separate\n{log}"
    );
    let convs: BTreeSet<_> = evs.iter().map(|e| &e.conversation_id).collect();
    assert_eq!(convs.len(), 2);
}

#[test]
fn s67_claude_code_and_codex_preserve_roles_order_tools_timestamps_session() {
    let t = tmp("s67");
    let cc = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/claude-code/tool_calls.jsonl"
    );
    let cx = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/codex/tool_calls.jsonl"
    );
    write(
        &t.join("in/.claude/projects/p/tool_calls.jsonl"),
        std::fs::read(cc).unwrap(),
    );
    write(
        &t.join("in/.codex/sessions/2026/04/01/rollout-2026-04-01T10-10-00-tool_calls.jsonl"),
        std::fs::read(cx).unwrap(),
    );
    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &[]);
    assert_eq!(code, 0, "{log}");
    let evs = events(&out);

    // --- Claude Code ---
    let mut claude: Vec<&Event> = evs.iter().filter(|e| e.provider == "anthropic").collect();
    claude.sort_by_key(|e| e.seq);
    let src: Vec<Value> = std::fs::read_to_string(cc)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let src_msgs: Vec<&Value> = src
        .iter()
        .filter(|v| matches!(v["type"].as_str(), Some("user" | "assistant")))
        .collect();
    let tool_use_blocks = src_msgs
        .iter()
        .flat_map(|v| {
            v["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|b| b["type"] == "tool_use")
        .count();
    let calls = |es: &[&Event]| {
        es.iter()
            .flat_map(|e| &e.content)
            .filter(|p| matches!(p, Part::ToolCall { .. }))
            .count()
    };
    assert!(tool_use_blocks > 0);
    assert_eq!(calls(&claude), tool_use_blocks);
    assert_eq!(claude[0].role, Role::User);
    assert_eq!(
        claude[0].session_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222")
    );
    assert!(claude
        .iter()
        .all(|e| e.session_id.as_deref() == Some("22222222-2222-4222-8222-222222222222")));
    // source order == seq order, timestamps verbatim
    let src_ts: Vec<String> = src
        .iter()
        .filter_map(|v| v["timestamp"].as_str().map(String::from))
        .collect();
    let got_ts: Vec<String> = claude.iter().filter_map(|e| e.timestamp.clone()).collect();
    assert_eq!(got_ts.len(), claude.len(), "every event keeps a timestamp");
    let norm = |s: &str| match s.trim_end_matches('Z').split_once('.') {
        Some((d, f)) if f.bytes().all(|b| b == b'0') => d.to_string(),
        _ => s.trim_end_matches('Z').to_string(),
    };
    let mut it = src_ts.iter().map(|s| norm(s));
    for g in &got_ts {
        let g = norm(g);
        assert!(it.any(|s| s == g), "timestamp {g} out of source order");
    }
    let roles: Vec<Role> = claude.iter().map(|e| e.role).collect();
    assert!(roles.contains(&Role::Assistant) && roles.contains(&Role::Tool));

    // --- Codex ---
    let mut codex: Vec<&Event> = evs.iter().filter(|e| e.provider == "openai").collect();
    codex.sort_by_key(|e| e.seq);
    let csrc: Vec<Value> = std::fs::read_to_string(cx)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let call_items = csrc
        .iter()
        .filter(|v| v["type"] == "response_item")
        .filter(|v| {
            matches!(
                v["payload"]["type"].as_str(),
                Some("function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call")
            )
        })
        .count();
    assert!(call_items > 0);
    assert_eq!(calls(&codex), call_items);
    assert!(codex
        .iter()
        .all(|e| e.session_id.as_deref() == Some("019a0002-aaaa-7bbb-8ccc-000000000002")));
    assert!(codex.iter().any(|e| e.role == Role::User
        && e.content.iter().any(|p| p
            .as_text()
            .is_some_and(|t| t.contains("list files then patch")))));
    assert!(
        codex.windows(2).all(|w| w[0].timestamp <= w[1].timestamp),
        "codex order follows source"
    );
    assert!(codex.iter().any(|e| e.role == Role::Tool));
}

#[test]
fn s68_same_remote_on_two_machines_shares_repository_id() {
    let t = tmp("s68");
    let (ma, mb) = (t.join("in/machine-a"), t.join("in/machine-b"));
    let remote_a = "git@github.com:acme/widgets.git";
    let remote_b = "https://github.com/acme/widgets";
    write(
        &ma.join(".codex/sessions/2026/04/01/rollout-a.jsonl"),
        codex_rollout(
            "019a0010-aaaa-7bbb-8ccc-00000000000a",
            "/home/user-a/work/widgets",
            remote_a,
            "main",
        ),
    );
    write(
        &mb.join(".codex/sessions/2026/04/01/rollout-b.jsonl"),
        codex_rollout(
            "019a0010-aaaa-7bbb-8ccc-00000000000b",
            "C:\\Users\\user-b\\src\\widgets",
            remote_b,
            "dev",
        ),
    );
    let cfg = t.join("cfg.toml");
    write(
        &cfg,
        format!(
            "[[machine]]\nid = \"user-a-laptop\"\npaths = [{:?}]\n\n[[machine]]\nid = \"user-b-desktop\"\npaths = [{:?}]\nplatform = \"windows\"\n",
            ma.canonicalize().unwrap().to_str().unwrap(),
            mb.canonicalize().unwrap().to_str().unwrap()
        ),
    );
    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &["--config", cfg.to_str().unwrap()]);
    assert_eq!(code, 0, "{log}");
    let evs = events(&out);
    let repos: BTreeSet<_> = evs.iter().map(|e| e.repository_id.clone()).collect();
    assert_eq!(repos.len(), 1, "one shared repository_id: {repos:?}");
    assert!(repos.iter().next().unwrap().is_some());
    let machines: BTreeSet<_> = evs.iter().map(|e| e.machine_id.clone()).collect();
    assert_eq!(machines.len(), 2, "{machines:?}");
    let dirs: BTreeSet<_> = evs.iter().map(|e| e.working_directory.clone()).collect();
    assert_eq!(dirs.len(), 2, "paths stay distinct: {dirs:?}");
    let convs: BTreeSet<_> = evs.iter().map(|e| &e.conversation_id).collect();
    assert_eq!(convs.len(), 2);
}

#[test]
fn s69_deleted_worktree_imports_and_keeps_historical_metadata() {
    let t = tmp("s69");
    let gone = "/nonexistent/worktrees/feature-x-7d1c";
    write(
        &t.join("in/.claude/projects/p/s.jsonl"),
        claude_session(SESSION, gone, "feature-x", 1..=4),
    );
    write(
        &t.join("in/.codex/sessions/2026/04/01/rollout-gone.jsonl"),
        codex_rollout(
            "019a0020-aaaa-7bbb-8ccc-000000000001",
            gone,
            "https://github.com/acme/widgets",
            "feature-x",
        ),
    );
    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &[]);
    assert_eq!(code, 0, "{log}");
    let evs = events(&out);
    assert_eq!(evs.len(), 4 + 3, "{log}");
    assert!(evs
        .iter()
        .all(|e| e.working_directory.as_deref() == Some(gone)));
    assert!(
        evs.iter().all(|e| e.branch.as_deref() == Some("feature-x")),
        "branch kept"
    );
}

#[test]
fn s70_reimport_is_idempotent() {
    let t = tmp("s70");
    write(
        &t.join("in/.claude/projects/p/s.jsonl"),
        claude_session(SESSION, "/home/u/p", "main", 1..=20),
    );
    write(
        &t.join("in/.codex/sessions/2026/04/01/rollout-x.jsonl"),
        codex_rollout(
            "019a0030-aaaa-7bbb-8ccc-000000000001",
            "/home/u/q",
            "https://github.com/acme/q",
            "main",
        ),
    );
    let out = t.join("out");
    let (c1, l1) = import(&t.join("in"), &out, &[]);
    assert_eq!(c1, 0, "{l1}");
    let first = events(&out);
    let p1 = provenance_counts(&out);
    let ids1: BTreeSet<_> = first.iter().map(|e| e.event_id.clone()).collect();

    for extra in [&[][..], &["--resume"][..]] {
        let (c2, l2) = import(&t.join("in"), &out, extra);
        assert_eq!(c2, 0, "{l2}");
        let again = events(&out);
        let ids2: BTreeSet<_> = again.iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(ids1, ids2, "same event ids");
        assert_eq!(again.len(), first.len(), "no growth: {l2}");
        assert_eq!(
            provenance_counts(&out),
            p1,
            "provenance does not grow on identical re-import"
        );
    }
    // A fresh output yields the very same ids (deterministic).
    let out2 = t.join("out2");
    import(&t.join("in"), &out2, &[]);
    let ids3: BTreeSet<_> = events(&out2).iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids1, ids3);
}

#[test]
fn s71_damaged_sources_are_reported_and_others_continue() {
    let t = tmp("s71");
    write(
        &t.join("in/good/.claude/projects/p/good.jsonl"),
        claude_session(SESSION, "/home/u/p", "main", 1..=6),
    );
    // Malformed JSON in the middle of an otherwise valid session.
    let mut bad = claude_session(
        "5e55105e-0000-4000-8000-000000000002",
        "/home/u/p",
        "main",
        1..=4,
    );
    bad.push_str("{this is not json\n");
    bad.push_str(&claude_session(
        "5e55105e-0000-4000-8000-000000000002",
        "/home/u/p",
        "main",
        5..=6,
    ));
    write(&t.join("in/bad/.claude/projects/p/bad.jsonl"), bad);
    // A zip with one good entry and one entry whose stored bytes are corrupted.
    let zip_path = t.join("in/archives/backup.zip");
    std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
    {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        use std::io::Write;
        z.start_file(".claude/projects/p/a.jsonl", opts).unwrap();
        z.write_all(
            claude_session(
                "5e55105e-0000-4000-8000-000000000003",
                "/home/u/p",
                "main",
                1..=4,
            )
            .as_bytes(),
        )
        .unwrap();
        z.start_file(".claude/projects/p/b.jsonl", opts).unwrap();
        z.write_all(
            claude_session(
                "5e55105e-0000-4000-8000-000000000004",
                "/home/u/p",
                "main",
                1..=4,
            )
            .as_bytes(),
        )
        .unwrap();
        z.finish().unwrap();
    }
    let mut bytes = std::fs::read(&zip_path).unwrap();
    let marker = b"question number 3";
    let last = bytes
        .windows(marker.len())
        .rposition(|w| w == marker)
        .unwrap();
    bytes[last] ^= 0xff; // corrupt entry b only (CRC mismatch)
    std::fs::write(&zip_path, bytes).unwrap();
    // A truncated gzip tarball.
    write(
        &t.join("in/archives/broken.tar.gz"),
        [0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x00],
    );

    let out = t.join("out");
    let (code, log) = import(&t.join("in"), &out, &[]);
    assert_eq!(code, 0, "damage is reported, not fatal:\n{log}");

    let evs = events(&out);
    let sessions: BTreeSet<_> = evs.iter().filter_map(|e| e.session_id.clone()).collect();
    assert!(
        sessions.contains(SESSION),
        "good source imported: {sessions:?}"
    );
    assert!(
        sessions.contains("5e55105e-0000-4000-8000-000000000002"),
        "valid records of the damaged file imported"
    );
    assert!(
        sessions.contains("5e55105e-0000-4000-8000-000000000003"),
        "good zip entry imported"
    );

    let stats = report::stats(&out).unwrap();
    assert!(stats.accounting_holds(), "{stats:?}");
    assert!(stats.parse_errors >= 1, "malformed line counted: {stats:?}");
    let pe = std::fs::read_to_string(out.join("reports/PARSE_ERRORS.md")).unwrap();
    assert!(pe.contains("bad.jsonl"), "{pe}");
    let inv = std::fs::read_to_string(out.join("reports/SOURCE_INVENTORY.json")).unwrap();
    assert!(
        inv.contains("broken.tar.gz"),
        "corrupt archive is in the inventory"
    );
    assert!(
        stats.sources_failed + stats.sources_partial >= 2,
        "{stats:?}"
    );
}
