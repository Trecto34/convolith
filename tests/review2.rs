//! Regression tests for the second review round (B1-B3, N2, N3), driven through
//! the real `convolith` binary.

use convolith::report;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-r2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

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

fn import(input: &Path, out: &Path, extra: &[&str]) -> String {
    let mut a = vec![
        "import",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    a.extend_from_slice(extra);
    let (code, log) = cli(&a);
    assert_eq!(code, 0, "import must not be fatal:\n{log}");
    log
}

/// `validate` must pass; returns its text for context in failures.
fn assert_valid(out: &Path) {
    let (code, log) = cli(&["validate", out.to_str().unwrap()]);
    assert_eq!(code, 0, "validate:\n{log}");
}

fn config(dir: &Path, body: &str) -> String {
    let p = dir.join("cfg.toml");
    write(&p, body);
    p.to_str().unwrap().to_string()
}

fn ledger_count(out: &Path, sql: &str) -> u64 {
    let l = convolith::ledger::Ledger::open(
        &convolith::dataset::Layout { root: out.into() }.provenance_db(),
    )
    .unwrap();
    l.count(sql).unwrap()
}

fn conversation(i: usize) -> Value {
    json!({"id": format!("5e55105e-0000-4000-8000-00000000000{i}"), "title": format!("t{i}"), "current_node": "b",
        "mapping": {
            "a": {"id":"a","parent":null,"children":["b"],"message":{"author":{"role":"user"},
                "create_time":1700000000.0 + i as f64,"content":{"parts":[format!("question {i}")]}}},
            "b": {"id":"b","parent":"a","children":[],"message":{"author":{"role":"assistant"},
                "create_time":1700000001.0 + i as f64,"content":{"parts":[format!("answer {i}")]}}}}})
}

#[test]
fn b1_truncated_chatgpt_export_keeps_imported_events_consistent() {
    let t = tmp("b1-chatgpt");
    let full = Value::Array((0..3).map(conversation).collect()).to_string();
    // Cut inside the third conversation: two are complete, then a syntax error.
    let cut = full.rfind("00000000002").unwrap();
    write(&t.join("in/conversations.json"), &full.as_bytes()[..cut]);
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 4, "{stats:?}");
    assert!(stats.accounting_holds(), "{stats:?}");
    assert_eq!(ledger_count(&out, "select count(*) from conversation"), 2);
    // 4 imported events + 1 source-level failure.
    assert_eq!(
        ledger_count(&out, "select sum(records_examined) from source"),
        5
    );
    assert_eq!(
        ledger_count(&out, "select sum(records_imported) from source"),
        4
    );
    assert_eq!(
        ledger_count(&out, "select sum(records_failed) from source"),
        1
    );
    assert!(ledger_count(&out, "select count(*) from parse_error") >= 1);
    assert_valid(&out);
}

#[test]
fn b1_generic_jsonl_with_invalid_utf8_line_imports_the_rest() {
    let t = tmp("b1-utf8");
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"role\":\"user\",\"content\":\"one\"}\n");
    body.extend_from_slice(b"{\"role\":\"user\",\"content\":\"bad \xff\xfe\"}\n");
    body.extend_from_slice(b"{\"role\":\"assistant\",\"content\":\"three\"}\n");
    write(&t.join("in/chat.jsonl"), body);
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 2, "{stats:?}");
    assert!(stats.accounting_holds(), "{stats:?}");
    assert_eq!(
        ledger_count(&out, "select sum(records_failed) from source"),
        1
    );
    assert!(ledger_count(&out, "select count(*) from parse_error") >= 1);
    assert_valid(&out);
}

#[test]
fn b2_generic_jsonl_oversize_line_is_one_failed_record() {
    let t = tmp("b2");
    let huge = format!(r#"{{"role":"user","content":"{}"}}"#, "x".repeat(5000));
    write(
        &t.join("in/chat.jsonl"),
        format!(
            "{}\n{huge}\n{}\n",
            r#"{"role":"user","content":"before"}"#, r#"{"role":"assistant","content":"after"}"#
        ),
    );
    let cfg = config(&t, "[limits]\nmax_record_bytes = 400\n");
    let out = t.join("out");
    import(&t.join("in"), &out, &["--config", &cfg]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 2, "{stats:?}");
    assert!(stats.accounting_holds(), "{stats:?}");
    assert_eq!(
        ledger_count(&out, "select sum(records_failed) from source"),
        1
    );
    assert_valid(&out);
}

#[test]
fn b3_shipped_compact_array_fixture_is_detected_and_parsed() {
    let t = tmp("b3-fixture");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generic/messages.json");
    write(&t.join("in/messages.json"), std::fs::read(fixture).unwrap());
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 2, "{stats:?}");
    assert!(stats.accounting_holds(), "{stats:?}");
    // The third element has non-text content: counted as skipped, not lost.
    assert_eq!(
        ledger_count(&out, "select sum(records_skipped) from source"),
        1
    );
    assert_valid(&out);
}

#[test]
fn b3_ndjson_saved_as_json_is_routed_to_the_line_reader() {
    let t = tmp("b3-ndjson");
    write(
        &t.join("in/chat.json"),
        "{\"role\":\"user\",\"content\":\"one\"}\n{\"role\":\"assistant\",\"content\":\"two\"}\n",
    );
    let out = t.join("out");
    import(&t.join("in"), &out, &[]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 2, "{stats:?}");
    assert_eq!(stats.parse_errors, 0, "{stats:?}");
    assert_valid(&out);
}

#[test]
fn b3_oversize_array_element_fails_the_source_but_keeps_earlier_events() {
    let t = tmp("b3-oversize");
    let huge = "x".repeat(5000);
    write(
        &t.join("in/chat.json"),
        format!(
            r#"[{{"role":"user","content":"small"}},{{"role":"user","content":"{huge}"}},{{"role":"user","content":"never"}}]"#
        ),
    );
    let cfg = config(&t, "[limits]\nmax_record_bytes = 400\n");
    let out = t.join("out");
    import(&t.join("in"), &out, &["--config", &cfg]);

    let stats = report::stats(&out).unwrap();
    assert_eq!(stats.events_in_shards, 1, "{stats:?}");
    assert!(stats.accounting_holds(), "{stats:?}");
    assert_eq!(ledger_count(&out, "select count(*) from conversation"), 1);
    assert_valid(&out);
}

#[test]
fn n2_discovery_honors_configured_archive_limits() {
    let t = tmp("n2");
    let zip_path = t.join("in/backup.zip");
    std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
    {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        z.start_file("chat.jsonl", opts).unwrap();
        for i in 0..20 {
            writeln!(z, r#"{{"role":"user","content":"message number {i}"}}"#).unwrap();
        }
        z.finish().unwrap();
    }
    let with_default = t.join("out-default");
    import(&t.join("in"), &with_default, &[]);
    assert_eq!(report::stats(&with_default).unwrap().events_in_shards, 20);

    let cfg = config(&t, "[limits]\nmax_stage_bytes = 100\n");
    let limited = t.join("out-limited");
    // An empty dataset fails `validate`, so the exit code is not asserted.
    cli(&[
        "import",
        t.join("in").to_str().unwrap(),
        "-o",
        limited.to_str().unwrap(),
        "--config",
        &cfg,
    ]);
    assert_eq!(
        report::stats(&limited).unwrap().events_in_shards,
        0,
        "a 100-byte staging cap must reject the member"
    );
}

#[test]
fn n3_reimporting_the_same_conflicting_pair_does_not_grow_conflicts() {
    let t = tmp("n3");
    let session = "5e55105e-0000-4000-8000-000000000001";
    let line = |text: &str| {
        format!(
            "{}\n",
            json!({"parentUuid": null, "isSidechain": false, "cwd": "/home/u/p",
                "sessionId": session, "gitBranch": "main", "uuid": "5e55105e-0001-4000-8000-000000000001",
                "timestamp": "2026-03-01T10:00:01.000Z", "type": "user",
                "message": {"role": "user", "content": text}})
        )
    };
    let file = t.join("in/.claude/projects/p/s.jsonl");
    let out = t.join("out");
    write(&file, line("original text"));
    import(&t.join("in"), &out, &[]);
    assert_eq!(report::stats(&out).unwrap().conflicts, 0);

    write(&file, line("edited text"));
    import(&t.join("in"), &out, &[]);
    let first = ledger_count(&out, "select count(*) from conflict");
    assert_eq!(first, 1, "the differing variant is one conflict");

    import(&t.join("in"), &out, &[]);
    assert_eq!(
        ledger_count(&out, "select count(*) from conflict"),
        first,
        "identical re-import must not add conflict rows"
    );
    assert_eq!(report::stats(&out).unwrap().conflicts, first);
}
