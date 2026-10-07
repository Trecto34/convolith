//! Feature tests: local discovery, LevelDB, identity aliases, artifacts and
//! secrets, each driven through the real binary where a CLI path exists.

use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::discover::{local_candidates, LocalOs};
use convolith::model::{ArtifactRecord, Event, Part};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-feat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn cli_env(args: &[&str], env: &[(&str, &str)], clear: &[&str]) -> (i32, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_convolith"));
    c.args(args);
    for k in clear {
        c.env_remove(k);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    // A synthetic HOME must also isolate Windows discovery from real stores.
    #[cfg(windows)]
    if let Some((_, home)) = env.iter().find(|(k, _)| *k == "HOME") {
        c.env("USERPROFILE", home)
            .env("APPDATA", Path::new(home).join(".config"))
            .env("LOCALAPPDATA", Path::new(home).join(".local/share"));
    }
    let o = c.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

fn cli(args: &[&str]) -> (i32, String) {
    cli_env(args, &[], &[])
}

/// Import and require a passing `validate` (datasets with no events fail the
/// layout check by design, so those use [`import_only`]).
fn import(input: &Path, out: &Path, extra: &[&str]) -> String {
    let log = import_only(input, out, extra);
    let (code, v) = cli(&["validate", out.to_str().unwrap()]);
    assert_eq!(code, 0, "validate:\n{v}");
    log
}

fn import_only(input: &Path, out: &Path, extra: &[&str]) -> String {
    let mut a = vec![
        "import",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    a.extend_from_slice(extra);
    let (code, log) = cli(&a);
    // Exit 1 is validate's verdict on a dataset with no events; not an import failure.
    assert!(
        code == 0 || (code == 1 && events(out).is_empty()),
        "import:\n{log}"
    );
    log
}

fn read_all<T: for<'de> serde::Deserialize<'de>>(dir: &Path, name: &str) -> Vec<T> {
    let mut shards: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name().to_string_lossy() == name
                || (name.is_empty() && e.file_name().to_string_lossy().ends_with(".jsonl.zst"))
        })
        .map(|e| e.into_path())
        .collect();
    shards.sort();
    let mut all = Vec::new();
    for s in shards {
        read_jsonl_zst::<T>(
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

fn events(out: &Path) -> Vec<Event> {
    read_all(&Layout { root: out.into() }.events_dir(), "")
}

fn artifacts(out: &Path) -> Vec<ArtifactRecord> {
    read_all(
        &Layout { root: out.into() }.aggregates_dir(),
        "artifacts.jsonl.zst",
    )
}

fn inventory(out: &Path) -> Vec<Value> {
    let v: Value =
        serde_json::from_slice(&std::fs::read(out.join("reports/SOURCE_INVENTORY.json")).unwrap())
            .unwrap();
    v["sources"].as_array().unwrap().clone()
}

fn ledger_count(out: &Path, sql: &str) -> u64 {
    convolith::ledger::Ledger::open(&Layout { root: out.into() }.provenance_db())
        .unwrap()
        .count(sql)
        .unwrap()
}

const SID: &str = "5e55105e-0000-4000-8000-000000000001";

/// A Claude Code session: user prompt, assistant tool call, tool result.
fn claude_tool_session(cwd: &str, tool_output: &str, user_text: &str) -> String {
    claude_tool_session_sid(SID, cwd, tool_output, user_text)
}

/// Event uuids derive from the session id, so distinct sessions never collide.
fn claude_tool_session_sid(sid: &str, cwd: &str, tool_output: &str, user_text: &str) -> String {
    let u = |n: u64| format!("{}-0000-4000-8000-{n:012}", &sid[28..]);
    let lines = [
        json!({"parentUuid":null,"isSidechain":false,"cwd":cwd,"sessionId":sid,"uuid":u(1),
            "timestamp":"2026-03-01T10:00:01.000Z","type":"user","message":{"role":"user","content":user_text}}),
        json!({"parentUuid":u(1),"isSidechain":false,"cwd":cwd,"sessionId":sid,"uuid":u(2),
            "timestamp":"2026-03-01T10:00:02.000Z","type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-5",
            "content":[{"type":"tool_use","id":"toolu_01AAAAAAAAAAAAAAAAAAAAAA","name":"Bash","input":{"command":"cat big.log"}}]}}),
        json!({"parentUuid":u(2),"isSidechain":false,"cwd":cwd,"sessionId":sid,"uuid":u(3),
            "timestamp":"2026-03-01T10:00:03.000Z","type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_01AAAAAAAAAAAAAAAAAAAAAA","content":tool_output,"is_error":false}]}}),
    ];
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn cfg(dir: &Path, body: &str) -> String {
    let p = dir.join("cfg.toml");
    write(&p, body);
    p.to_str().unwrap().to_string()
}

// ---- (1) discover --local --------------------------------------------------

#[test]
fn local_candidates_follow_each_platforms_conventions() {
    let win = |k: &str| match k {
        "USERPROFILE" => Some("C:\\Users\\a".to_string()),
        "APPDATA" => Some("C:\\Users\\a\\AppData\\Roaming".to_string()),
        "LOCALAPPDATA" => Some("D:\\Local".to_string()),
        _ => None,
    };
    let w = local_candidates(LocalOs::Windows, &win);
    let paths: Vec<String> = w
        .iter()
        .map(|r| r.path.to_string_lossy().into_owned())
        .collect();
    assert!(paths
        .iter()
        .any(|p| p.ends_with("projects") && p.contains(".claude")));
    assert!(paths
        .iter()
        .any(|p| p.starts_with("C:\\Users\\a\\AppData\\Roaming") && p.ends_with("Claude")));
    assert!(
        paths.iter().any(|p| p.starts_with("D:\\Local")),
        "LOCALAPPDATA honoured: {paths:?}"
    );

    let lin = |k: &str| match k {
        "HOME" => Some("/home/a".to_string()),
        "XDG_CONFIG_HOME" => Some("/xdg/cfg".to_string()),
        "XDG_DATA_HOME" => Some("/xdg/data".to_string()),
        _ => None,
    };
    let l = local_candidates(LocalOs::Linux, &lin);
    let paths: Vec<PathBuf> = l.iter().map(|r| r.path.clone()).collect();
    assert!(paths.contains(&PathBuf::from("/home/a/.claude/projects")));
    assert!(paths.contains(&PathBuf::from("/xdg/cfg/claude/projects")));
    assert!(paths.contains(&PathBuf::from("/xdg/cfg/Claude")));
    assert!(paths.contains(&PathBuf::from("/xdg/data/Claude")));
    assert!(
        !paths.iter().any(|p| p.starts_with("/home/a/.config")),
        "XDG override respected"
    );
    // No home, no guesses.
    assert!(local_candidates(LocalOs::Linux, &|_| None).is_empty());
}

#[test]
fn discover_local_lists_recognised_sessions_and_unresolved_leveldb() {
    let t = tmp("disc-local");
    let home = t.join("home");
    write(
        &home.join(".claude/projects/-p/s.jsonl"),
        claude_tool_session("/home/a/p", "ok", "hi"),
    );
    // Electron profile of a desktop client: caches skipped, LevelDB unresolved.
    let ldb = home.join(".config/Claude/Local Storage/leveldb");
    write(&ldb.join("CURRENT"), "MANIFEST-000001\n");
    write(&ldb.join("MANIFEST-000001"), b"\x01manifest");
    write(
        &ldb.join("000003.log"),
        b"\x00_https://claude.ai\x00\x01k\x00v",
    );
    write(
        &home.join(".config/Claude/Cache/blob.json"),
        "{\"role\":\"user\"}\n",
    );
    let (code, out) = cli_env(
        &["discover", "--local"],
        &[("HOME", home.to_str().unwrap())],
        &[
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "USERPROFILE",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("claude_code") && l.contains("s.jsonl")),
        "{out}"
    );
    assert!(
        out.lines().any(|l| l.starts_with("unresolved")
            && l.contains("leveldb")
            && l.contains("anthropic")),
        "{out}"
    );
    assert!(
        !out.contains("blob.json"),
        "cache dirs are not walked:\n{out}"
    );
}

#[test]
fn a_file_named_chat_db_is_not_proof_of_an_ai_client() {
    let t = tmp("chatdb");
    let input = t.join("in");
    write(
        &input.join("chat.db"),
        b"SQLite format 3\0not a conversation store at all",
    );
    write(&input.join("Messages/chat.db"), b"\x00\x01\x02random");
    let (code, out) = cli(&["discover", input.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(
        out.lines()
            .filter(|l| l.contains("chat.db") && l.starts_with("-"))
            .count(),
        2,
        "{out}"
    );
    let o = t.join("out");
    import_only(&input, &o, &[]);
    assert!(events(&o).is_empty());
    let inv = inventory(&o);
    assert_eq!(
        inv.iter().filter(|r| r["status"] == "unsupported").count(),
        2,
        "{inv:?}"
    );
}

// ---- (2) LevelDB -----------------------------------------------------------

fn make_ldb(dir: &Path, origin: &str) {
    write(&dir.join("CURRENT"), "MANIFEST-000001\n");
    write(&dir.join("MANIFEST-000001"), b"\x01manifest");
    write(&dir.join("LOCK"), b"");
    write(
        &dir.join("000003.log"),
        format!(
            "\u{0}_{origin}\u{0}\u{1}history\u{0}{{\"role\":\"user\",\"content\":\"leaked?\"}}"
        )
        .as_bytes(),
    );
}

#[test]
fn leveldb_is_inventoried_unresolved_never_turned_into_conversations() {
    let t = tmp("ldb");
    let input = t.join("in");
    make_ldb(
        &input.join("app-a/Local Storage/leveldb"),
        "https://example.invalid",
    );
    make_ldb(
        &input.join("app-b/IndexedDB/x.leveldb"),
        "https://chatgpt.com",
    );
    let snapshot = |d: &Path| -> BTreeMap<String, Vec<u8>> {
        walkdir::WalkDir::new(d)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                (
                    e.path().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect()
    };
    let before = snapshot(&input);
    let out = t.join("out");
    import_only(&input, &out, &[]);
    assert_eq!(
        snapshot(&input),
        before,
        "the source tree is never modified"
    );
    assert!(
        events(&out).is_empty(),
        "no conversation without a decoder that proves the provider"
    );
    assert_eq!(
        ledger_count(&out, "select count(*) from source where format='leveldb' and status='unsupported' and notes like '%unresolved%'"),
        2
    );
    assert_eq!(
        ledger_count(
            &out,
            "select count(*) from source where format='leveldb' and notes like '%openai%'"
        ),
        1
    );
    assert_eq!(ledger_count(&out, "select count(*) from source where format='leveldb' and notes like '%no provider evidence%'"), 1);
    // The component files are not separately offered to the parsers.
    assert_eq!(
        ledger_count(
            &out,
            "select count(*) from source where display_path like '%000003.log'"
        ),
        0
    );
}

// ---- (3) aliases and repository identity -----------------------------------

fn project_ids(out: &Path) -> BTreeSet<Option<String>> {
    events(out).into_iter().map(|e| e.project_id).collect()
}

fn import_cwds(
    tag: &str,
    cwds: &[&str],
    config: Option<&str>,
) -> (PathBuf, BTreeSet<Option<String>>) {
    let t = tmp(tag);
    for (i, cwd) in cwds.iter().enumerate() {
        let sid = format!("5e55105e-0000-4000-8000-00000000010{i}");
        let body = claude_tool_session_sid(&sid, cwd, "ok", &format!("hello {i}"));
        write(
            &t.join(format!("in/m{i}/.claude/projects/p/{sid}.jsonl")),
            body,
        );
    }
    let mut extra = Vec::new();
    let c;
    if let Some(body) = config {
        c = cfg(&t, body);
        extra = vec!["--config", &c];
    }
    let out = t.join("out");
    import(&t.join("in"), &out, &extra);
    let ids = project_ids(&out);
    (out, ids)
}

#[test]
fn project_alias_unifies_windows_wsl_and_unc_spellings_only_when_declared() {
    let spellings = ["C:\\src\\foo", "/mnt/c/src/foo", "\\\\nas\\code\\foo"];
    let (_, plain) = import_cwds("alias-none", &spellings, None);
    assert_eq!(
        plain.len(),
        2,
        "C:\\ and /mnt/c are one path; the UNC share is not merged: {plain:?}"
    );
    let config = "[[project_alias]]\nproject = \"foo\"\npaths = [\"c:\\\\SRC\\\\foo\", \"//NAS/code/foo\"]\n";
    let (out, aliased) = import_cwds("alias-set", &spellings, Some(config));
    assert_eq!(aliased.len(), 1, "{aliased:?}");
    assert_eq!(
        ledger_count(&out, "select count(*) from project where name='foo'"),
        1
    );
}

#[test]
fn similar_paths_are_never_fuzzily_merged() {
    let (_, ids) = import_cwds(
        "nofuzzy",
        &[
            "/home/u/Foo",
            "/home/u/foo",
            "/home/u/foo-old",
            "/home/v/foo",
        ],
        Some("[[project_alias]]\nproject = \"foo\"\npaths = [\"/home/u/foo\"]\n"),
    );
    // alias covers exactly /home/u/foo (and below); Foo, foo-old and /home/v/foo stay apart.
    assert_eq!(ids.len(), 4, "{ids:?}");
}

#[test]
fn machine_prefix_matches_on_component_boundaries() {
    let t = tmp("machine");
    write(
        &t.join("in/host-a/.claude/projects/p/a.jsonl"),
        claude_tool_session("/w", "ok", "a"),
    );
    write(
        &t.join("in/host-a-old/.claude/projects/p/b.jsonl"),
        claude_tool_session_sid("5e55105e-0000-4000-8000-000000000002", "/w", "ok", "b"),
    );
    let prefix = t.join("in/host-a");
    let c = cfg(
        &t,
        &format!(
            "[[machine]]\nid = \"gpu\"\npaths = [{:?}]\n",
            prefix.canonicalize().unwrap().to_str().unwrap()
        ),
    );
    let out = t.join("out");
    import(&t.join("in"), &out, &["--config", &c]);
    let m: BTreeSet<_> = events(&out).into_iter().map(|e| e.machine_id).collect();
    assert_eq!(
        m,
        BTreeSet::from([Some("gpu".to_string()), None]),
        "host-a-old must not match host-a"
    );
}

#[test]
fn git_remote_is_read_as_data_and_hooks_never_run() {
    let t = tmp("gitdata");
    let home = t.join("home");
    let repo = home.join("work/widgets");
    let marker = t.join("HOOK_RAN");
    write(&repo.join(".git/config"), "[core]\n\tfsmonitor = touch HOOK_RAN\n[remote \"origin\"]\n\turl = git@github.com:acme/widgets.git\n");
    write(
        &repo.join(".git/hooks/post-checkout"),
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    write(
        &home.join(".claude/projects/p/s.jsonl"),
        claude_tool_session(repo.join("src").to_str().unwrap(), "ok", "hi"),
    );
    // A dotfiles repo at $HOME must not become every project's identity.
    write(
        &home.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@github.com:me/dotfiles.git\n",
    );
    write(
        &home.join(".claude/projects/q/t.jsonl"),
        claude_tool_session_sid(
            "5e55105e-0000-4000-8000-000000000003",
            home.join("scratch").to_str().unwrap(),
            "ok",
            "hi",
        ),
    );
    let out = t.join("out");
    let (code, log) = cli_env(
        &[
            "import",
            home.join(".claude").to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
        &[("HOME", home.to_str().unwrap())],
        &["USERPROFILE"],
    );
    assert_eq!(code, 0, "{log}");
    assert!(!marker.exists(), "no git hook or helper may run");
    let evs = events(&out);
    let with_repo: Vec<_> = evs
        .iter()
        .filter(|e| {
            e.working_directory
                .as_deref()
                .is_some_and(|w| w.contains("widgets"))
        })
        .collect();
    assert!(
        !with_repo.is_empty() && with_repo.iter().all(|e| e.repository_id.is_some()),
        "repository identity from .git/config"
    );
    let ids: BTreeSet<_> = evs.iter().filter_map(|e| e.repository_id.clone()).collect();
    assert_eq!(
        ids.len(),
        1,
        "the dotfiles repo above $HOME is not evidence: {ids:?}"
    );
    // Same remote, other spelling, other machine: same repository.
    let again = convolith::identity::resolve_project(
        &convolith::config::Config::default(),
        Some("C:\\x\\widgets"),
        Some("https://github.com/acme/widgets"),
        None,
    )
    .unwrap();
    assert_eq!(again.repository_id.as_ref(), ids.iter().next());
}

// ---- (4) artifacts ---------------------------------------------------------

#[test]
fn large_tool_output_spills_to_a_content_addressed_artifact_with_metadata() {
    let t = tmp("spill");
    let big = "line of tool output\n".repeat(400); // 8000 bytes
    write(
        &t.join("in/a/.claude/projects/p/s.jsonl"),
        claude_tool_session("/w", &big, "run it"),
    );
    // The same output seen in a second, different session: stored once.
    write(
        &t.join("in/b/.claude/projects/p/u.jsonl"),
        claude_tool_session_sid(
            "5e55105e-0000-4000-8000-000000000009",
            "/w",
            &big,
            "run it again",
        ),
    );
    let c = cfg(&t, "[limits]\nmax_inline_text_bytes = 1024\n");
    let out = t.join("out");
    import(&t.join("in"), &out, &["--config", &c]);

    let evs = events(&out);
    let results: Vec<&Part> = evs
        .iter()
        .flat_map(|e| e.content.iter())
        .filter(|p| matches!(p, Part::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 2);
    let mut art_ids = BTreeSet::new();
    for r in &results {
        let Part::ToolResult { output, .. } = r else {
            unreachable!()
        };
        let aref = &output["artifact_ref"];
        assert_eq!(aref["bytes"], big.len());
        assert!(output["inline_preview"].as_str().unwrap().len() <= 1024);
        art_ids.insert(aref["artifact_id"].as_str().unwrap().to_string());
    }
    assert_eq!(art_ids.len(), 1, "identical output is one artifact");

    let arts = artifacts(&out);
    assert_eq!(arts.len(), 1);
    let a = &arts[0];
    assert_eq!(a.size, big.len() as u64);
    assert_eq!(a.sha256, convolith::id::sha256_bytes(big.as_bytes()));
    assert_eq!(
        a.stored_path,
        format!("sha256/{}/{}", &a.sha256[..2], a.sha256)
    );
    assert_eq!(
        std::fs::read(out.join("artifacts").join(&a.stored_path)).unwrap(),
        big.as_bytes()
    );
    assert!(a.mime.is_some() && a.source_path.is_some());
    let tool_result_ids: BTreeSet<String> = evs
        .iter()
        .filter(|e| {
            e.content
                .iter()
                .any(|p| matches!(p, Part::ToolResult { .. }))
        })
        .map(|e| e.event_id.clone())
        .collect();
    assert_eq!(
        a.event_ids.iter().cloned().collect::<BTreeSet<_>>(),
        tool_result_ids,
        "related event ids"
    );
    // Reimport adds nothing.
    import(&t.join("in"), &out, &["--config", &c]);
    assert_eq!(artifacts(&out).len(), 1);
}

#[test]
fn small_tool_output_stays_inline_and_threshold_is_configurable() {
    let t = tmp("nospill");
    write(
        &t.join("in/.claude/projects/p/s.jsonl"),
        claude_tool_session("/w", "short", "hi"),
    );
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);
    assert!(artifacts(&out).is_empty());
    let c = cfg(&t, "[limits]\nmax_inline_text_bytes = 3\n");
    let out2 = t.join("out2");
    import(&t.join("in"), &out2, &["--config", &c]);
    assert!(
        !artifacts(&out2).is_empty(),
        "a lower threshold spills the same data"
    );
}

// ---- (5) secrets -----------------------------------------------------------

const KEY: &str = "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef";
const PEM_BODY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";
const PW: &str = "hunter2hunter2";

fn secret_session() -> String {
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{PEM_BODY}\n-----END PRIVATE KEY-----");
    let big = format!(
        "{}\nAuthorization: Bearer abcdefghijklmnop0123456789\n{pem}",
        "x".repeat(3000)
    );
    claude_tool_session("/w", &big, &format!("my key is {KEY} and password={PW}"))
}

fn tree_contains(root: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for e in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let bytes = std::fs::read(e.path()).unwrap();
        let found = bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        // Shards are zstd: check the decompressed form too.
        let dec = zstd::decode_all(&bytes[..])
            .map(|d| d.windows(needle.len()).any(|w| w == needle.as_bytes()))
            .unwrap_or(false);
        if found || dec {
            hits.push(e.path().display().to_string());
        }
    }
    hits
}

#[test]
fn default_policy_redacts_everywhere_including_spilled_artifacts_and_logs() {
    let t = tmp("secrets");
    write(&t.join("in/s/.claude/projects/p/s.jsonl"), secret_session());
    let c = cfg(&t, "[limits]\nmax_inline_text_bytes = 1024\n");
    let out = t.join("out");
    let log = import(&t.join("in"), &out, &["--config", &c]);
    for needle in [KEY, PEM_BODY, PW, "abcdefghijklmnop0123456789"] {
        assert!(!log.contains(needle), "{needle} reached the CLI output");
        let hits = tree_contains(&out, needle);
        assert!(hits.is_empty(), "{needle} stored in clear in {hits:?}");
    }
    let audit = std::fs::read_to_string(out.join("reports/PRIVACY_AUDIT.md")).unwrap();
    for kind in [
        "anthropic_key",
        "private_key",
        "authorization_header",
        "password_assignment",
    ] {
        assert!(
            audit.contains(kind),
            "PRIVACY_AUDIT counts {kind}:\n{audit}"
        );
    }
    assert!(audit.contains("at least one redaction"));
    let (code, s) = cli(&["search", out.to_str().unwrap(), "REDACTED"]);
    assert_eq!(code, 0);
    assert!(!s.contains(KEY) && !s.contains(PEM_BODY));
}

#[test]
fn preserve_is_opt_in_and_still_audits() {
    let t = tmp("preserve");
    write(&t.join("in/.claude/projects/p/s.jsonl"), secret_session());
    let out = t.join("out");
    let log = import(&t.join("in"), &out, &["--secret-policy", "preserve"]);
    assert!(
        !log.contains(KEY),
        "logs never carry secrets, whatever the policy"
    );
    assert!(
        !tree_contains(&out.join("data"), KEY).is_empty(),
        "preserve keeps the canonical text"
    );
    let audit = std::fs::read_to_string(out.join("reports/PRIVACY_AUDIT.md")).unwrap();
    assert!(audit.contains("anthropic_key"));
    assert!(!audit.contains(KEY));
    let _ = Value::Null;
}

/// `rebuild_aggregates` once referenced columns that `event_index` never had.
/// It must run on a real imported archive and reproduce the import-time counts.
#[test]
fn rebuild_aggregates_runs_on_an_imported_archive_and_reproduces_counts() {
    let t = tmp("rebuild-agg");
    for (i, cwd) in ["/work/alpha", "/work/beta"].iter().enumerate() {
        let sid = format!("5e55105e-0000-4000-8000-00000000020{i}");
        let body = claude_tool_session_sid(&sid, cwd, "ok", &format!("hi {i}"));
        write(&t.join(format!("in/.claude/projects/p/{sid}.jsonl")), body);
    }
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);
    let snap = |l: &convolith::ledger::Ledger| {
        [
            "select count(*) from conversation_source",
            "select count(*) from project_path",
            "select count(*) from conversation where event_count = 3",
            "select ifnull(sum(event_count),0) from project",
        ]
        .map(|q| l.count(q).unwrap())
    };
    let mut l =
        convolith::ledger::Ledger::open(&Layout { root: out.clone() }.provenance_db()).unwrap();
    let before = snap(&l);
    assert_eq!(before[0], 2);
    assert_eq!(before[1], 2, "one project path per working directory");
    assert_eq!(before[2], 2);
    assert_eq!(before[3], 6);
    l.rebuild_aggregates().expect("rebuild_aggregates");
    assert_eq!(snap(&l), before);
}

/// Collection must not enumerate agent work files or chunks under `~/.gemini`,
/// yet still find every transcript and chat session; `discover PATH` (the
/// manual/forensic path) still lists everything.
#[test]
fn gemini_collection_walks_only_known_transcript_paths() {
    let t = tmp("gemini-narrow");
    let home = t.join("home");
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let gem = home.join(".gemini");
    let copy = |from: &str, to: &str| {
        write(&gem.join(to), std::fs::read(fx.join(from)).unwrap());
    };
    let brain = "antigravity-cli/brain/5e1f0c2a-7b3d-4a9e-8c10-2d4f6a8b0c12/.system_generated";
    copy(
        &format!("antigravity/home/.gemini/{brain}/logs/transcript_full.jsonl"),
        &format!("{brain}/logs/transcript_full.jsonl"),
    );
    let chats = "gemini-cli/home/.gemini/tmp/demo-project";
    copy(
        &format!("{chats}/.project_root"),
        "tmp/demo-project/.project_root",
    );
    copy(
        &format!("{chats}/chats/session-2026-02-03T10-00-0b7c1d2e.jsonl"),
        "tmp/demo-project/chats/session-2026-02-03T10-00-0b7c1d2e.jsonl",
    );
    for decoy in [
        format!("{brain}/logs/transcript.jsonl"),
        format!("{brain}/logs/chunks/transcript_full/0.jsonl"),
        format!("{brain}/worktrees/w/graphify-out/cache/a.json"),
        format!("{brain}/steps/2/out.txt"),
        "antigravity-cli/brain/5e1f0c2a-7b3d-4a9e-8c10-2d4f6a8b0c12/notes.md".into(),
        "antigravity-cli/conversations/x.db".into(),
        "antigravity-cli/log/cli.log".into(),
        "antigravity-cli/history.jsonl".into(),
        "tmp/demo-project/logs.json".into(),
        "tmp/demo-project/src/big.rs".into(),
        "settings.json".into(),
    ] {
        write(&gem.join(decoy), "{}\n");
    }
    let h = home.to_str().unwrap();
    let (code, log) = cli_env(
        &["discover", "--local"],
        &[("HOME", h)],
        &["XDG_CONFIG_HOME"],
    );
    assert_eq!(code, 0, "{log}");
    let listed = |log: &str, name: &str| log.lines().any(|l| l.contains(name));
    for found in [
        "transcript_full.jsonl",
        "session-2026-02-03T10-00-0b7c1d2e.jsonl",
        ".project_root",
    ] {
        assert!(listed(&log, found), "{found} missing:\n{log}");
    }
    for decoy in [
        "transcript.jsonl ",
        "0.jsonl",
        "a.json",
        "out.txt",
        "notes.md",
        "x.db",
        "cli.log",
        "history.jsonl",
        "logs.json",
        "big.rs",
        "settings.json",
    ] {
        assert!(!listed(&log, decoy), "{decoy} must not be walked:\n{log}");
    }
    // Manual import of the same tree still inventories everything.
    let (code, all) = cli(&["discover", gem.to_str().unwrap()]);
    assert_eq!(code, 0, "{all}");
    for decoy in ["0.jsonl", "big.rs", "cli.log", "notes.md"] {
        assert!(listed(&all, decoy), "{decoy} must be inventoried:\n{all}");
    }
}
