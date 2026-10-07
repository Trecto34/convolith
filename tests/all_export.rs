//! `convolith all`: ordering, determinism, dedup, provenance, read-only.
//! Synthetic fixtures only.

use convolith::dataset::{JsonlZstWriter, Layout};
use convolith::ledger::Ledger;
use convolith::model::{Event, EventType, Part, Role};
use convolith::timeutil::TimestampConfidence;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_convolith");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

fn tmp(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-all-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ev(id: &str, conv: &str, seq: u64, ts: Option<&str>, conf: TimestampConfidence) -> Event {
    Event {
        schema_version: 1,
        event_id: id.into(),
        conversation_id: conv.into(),
        session_id: None,
        parent_event_id: None,
        seq,
        timestamp: ts.map(str::to_string),
        timestamp_original: ts.map(str::to_string),
        timestamp_confidence: conf,
        role: Role::User,
        event_type: EventType::Message,
        provider: "p".into(),
        application: "a".into(),
        model: None,
        agent: None,
        machine_id: None,
        project_id: None,
        repository_id: None,
        working_directory: None,
        branch: None,
        commit: None,
        worktree_id: None,
        content: vec![Part::text("hi")],
        metadata: Default::default(),
        redactions: vec![],
        provenance: vec![],
    }
}

/// Archive with hand-written shards (one event list per shard).
fn archive(root: &Path, shards: &[Vec<Event>]) {
    let layout = Layout {
        root: root.to_path_buf(),
    };
    layout.ensure().unwrap();
    Ledger::open(&layout.provenance_db()).unwrap();
    for (i, evs) in shards.iter().enumerate() {
        let rel = format!("data/events/part-{:06}.jsonl.zst", i + 1);
        let mut w = JsonlZstWriter::create(&root.join(&rel), 1).unwrap();
        for e in evs {
            w.write_record(e).unwrap();
        }
        w.finish(&rel, "run").unwrap();
    }
}

fn run_all(archive: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .arg("all")
        .arg(archive)
        .args(extra)
        .output()
        .unwrap()
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ids(v: &[Value]) -> Vec<&str> {
    v.iter().map(|e| e["event_id"].as_str().unwrap()).collect()
}

#[test]
fn order_ties_unknowns_dups_and_no_fabrication() {
    use TimestampConfidence::*;
    let d = tmp("order");
    let a = d.join("arch");
    archive(
        &a,
        &[
            vec![
                ev("e_late", "c1", 0, Some("2026-01-02T00:00:00Z"), Exact),
                ev("e_unk_b", "c2", 1, None, Unknown),
                ev("e_unk_a", "c2", 0, None, SequenceOnly),
                // same instant: exact beats derived regardless of seq/id
                ev(
                    "e_tie_d",
                    "c3",
                    0,
                    Some("2026-01-01T00:00:00Z"),
                    ProviderDerived,
                ),
                ev("e_tie_x", "c3", 5, Some("2026-01-01T00:00:00Z"), Exact),
                ev("e_tie_w", "c4", 2, Some("2026-01-01T00:00:00Z"), Exact),
                // offset form normalizes to the same instant
                ev("e_tie_v", "c4", 2, Some("2026-01-01T01:00:00+01:00"), Exact),
            ],
            vec![
                ev("e_early", "c1", 1, Some("2025-12-31T23:59:59.5Z"), Exact),
                ev("e_late", "c1", 0, Some("2026-01-02T00:00:00Z"), Exact), // duplicate
                ev("e_bad", "c5", 0, Some("not a time"), Exact),
            ],
        ],
    );
    let o1 = run_all(&a, &["--stdout"]);
    assert!(o1.status.success());
    let o2 = run_all(&a, &["--stdout"]);
    assert_eq!(o1.stdout, o2.stdout, "deterministic");
    let v = lines(&o1.stdout);
    assert_eq!(
        ids(&v),
        [
            "e_early", "e_tie_v", "e_tie_w", "e_tie_x", "e_tie_d", "e_late", "e_unk_a", "e_unk_b",
            "e_bad"
        ]
    );
    let mut seen = std::collections::HashSet::new();
    assert!(v
        .iter()
        .all(|e| seen.insert(e["event_id"].as_str().unwrap())));
    for id in ["e_unk_a", "e_unk_b", "e_bad"] {
        let e = v.iter().find(|e| e["event_id"] == id).unwrap();
        assert!(e["timestamp"].is_null(), "{id} must not get a time");
    }
    let by = |id: &str| v.iter().find(|e| e["event_id"] == id).unwrap().clone();
    assert_eq!(by("e_unk_a")["timestamp_confidence"], "sequence-only");
    assert_eq!(by("e_bad")["timestamp_confidence"], "unknown");
    assert_eq!(by("e_bad")["timestamp_original"], "not a time");
    assert_eq!(by("e_tie_d")["timestamp_confidence"], "derived");
    assert_eq!(by("e_tie_v")["timestamp"], "2026-01-01T00:00:00.000000000Z");
    std::fs::remove_dir_all(&d).ok();
}

fn import(paths: &[PathBuf], out: &Path) {
    for p in paths {
        let r = Command::new(BIN)
            .arg("import")
            .arg(p)
            .arg("-o")
            .arg(out)
            .output()
            .unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    }
}

fn checksums(root: &Path) -> Vec<(PathBuf, String)> {
    walk(root)
        .into_iter()
        .map(|p| {
            let h = convolith::id::sha256_file(&p).unwrap();
            (p, h)
        })
        .collect()
}

fn walk(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = walkdir::WalkDir::new(d)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    v.sort();
    v
}

#[test]
fn real_parsers_end_to_end() {
    let d = tmp("real");
    let srcs: Vec<PathBuf> = ["claude-code", "codex", "gemini-cli", "chatgpt", "pi"]
        .iter()
        .map(|n| Path::new(FIXTURES).join(n))
        .collect();
    let a = d.join("a");
    import(&srcs, &a);
    let before = checksums(&a);

    let out = d.join("out").join("all.jsonl");
    let f = run_all(&a, &["--output", out.to_str().unwrap()]);
    assert!(f.status.success(), "{}", String::from_utf8_lossy(&f.stderr));
    let file = std::fs::read(&out).unwrap();
    let so = run_all(&a, &["--stdout"]);
    assert_eq!(file, so.stdout, "--stdout == --output");
    assert!(!so.stderr.is_empty() && !String::from_utf8_lossy(&so.stdout).contains("event(s)"));
    assert_eq!(before, checksums(&a), "archive is read-only");
    assert!(!walk(&d.join("out"))
        .iter()
        .any(|p| p.to_string_lossy().contains(".tmp")));

    let v = lines(&file);
    assert!(v.len() > 20);
    let provs: std::collections::HashSet<_> =
        v.iter().map(|e| e["provider"].as_str().unwrap()).collect();
    assert!(provs.len() >= 4, "several providers: {provs:?}");
    assert!(v.iter().all(|e| e["schema"] == "convolith.all/v1"));

    // chronological; null timestamps trail, in (conversation, seq) order
    let ts: Vec<_> = v.iter().map(|e| e["timestamp"].as_str()).collect();
    let first_null = ts.iter().position(|t| t.is_none()).unwrap_or(ts.len());
    assert!(ts[first_null..].iter().all(|t| t.is_none()));
    let known: Vec<_> = ts[..first_null].iter().map(|t| t.unwrap()).collect();
    assert!(
        known.windows(2).all(|w| w[0] <= w[1]),
        "lexical == chronological"
    );
    let nulls = &v[first_null..];
    assert!(nulls.iter().all(|e| e["timestamp_confidence"] != "exact"));
    let key = |e: &Value| {
        (
            e["conversation_id"].as_str().unwrap().to_string(),
            e["seq"].as_u64().unwrap(),
        )
    };
    assert!(nulls.windows(2).all(|w| key(&w[0]) <= key(&w[1])));

    // no duplicates; provenance points at real fixture paths
    let mut seen = std::collections::HashSet::new();
    assert!(v
        .iter()
        .all(|e| seen.insert(e["event_id"].as_str().unwrap())));
    for e in &v {
        let refs = e["source_refs"].as_array().unwrap();
        assert!(!refs.is_empty(), "{}", e["event_id"]);
        let p = refs[0]["original_path"].as_str().unwrap();
        assert!(p.starts_with(FIXTURES) || p.contains("fixtures"), "{p}");
        assert!(refs[0]["observation_id"].is_i64());
    }

    // conversation boundaries: a conversation's events keep source order & carry metadata
    let mut per: std::collections::BTreeMap<&str, Vec<&Value>> = Default::default();
    for e in &v {
        per.entry(e["conversation_id"].as_str().unwrap())
            .or_default()
            .push(e);
    }
    for evs in per.values() {
        let c = &evs[0]["conversation"];
        assert!(c.is_object() && c["event_count"].as_u64().unwrap() >= 1);
        let mut seqs: Vec<_> = evs.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        seqs.sort();
        seqs.dedup();
        assert_eq!(seqs.len(), evs.len(), "seq unique within a conversation");
    }
    assert!(per.len() >= 5);

    // tool call <-> result link survives
    let mut calls = std::collections::HashSet::new();
    let mut results = vec![];
    for e in &v {
        for b in e["content"].as_array().unwrap() {
            match b["type"].as_str().unwrap() {
                "tool_call" => {
                    if let Some(id) = b["id"].as_str() {
                        calls.insert((e["conversation_id"].clone(), id.to_string()));
                    }
                }
                "tool_result" => {
                    if let Some(id) = b["call_id"].as_str() {
                        results.push((e["conversation_id"].clone(), id.to_string()));
                    }
                }
                _ => {}
            }
        }
    }
    assert!(!results.is_empty() && results.iter().any(|r| calls.contains(r)));

    // subagent relationship surfaced
    assert!(v.iter().any(|e| e["subagent"].is_object()));

    // insertion-order independence: import sources reversed, same event sequence
    let mut rev = srcs.clone();
    rev.reverse();
    let b = d.join("b");
    import(&rev, &b);
    let w = lines(&run_all(&b, &["--stdout"]).stdout);
    assert_eq!(ids(&v), ids(&w));
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn needs_exactly_one_target_and_a_real_archive() {
    let d = tmp("args");
    assert!(!run_all(&d, &[]).status.success());
    assert!(!run_all(&d, &["--stdout", "--output", "x"]).status.success());
    let r = run_all(&d, &["--stdout"]);
    assert!(!r.status.success());
    assert!(r.stdout.is_empty());
    std::fs::remove_dir_all(&d).ok();
    let _ = json!(null);
}
