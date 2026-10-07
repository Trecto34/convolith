//! Shared harness for the web-export parser tests. Fixtures are synthetic and
//! built here; no text comes from a real conversation.
#![allow(dead_code)]

use convolith::dataset::{read_jsonl_zst, Layout};
use convolith::model::Event;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-web-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

pub fn cli(args: &[&str]) -> (i32, String) {
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

/// `convolith import INPUT -o OUT`; returns the CLI log (asserts success).
pub fn import(input: &Path, out: &Path) -> String {
    let (code, log) = cli(&[
        "import",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{log}");
    log
}

/// Like [`import`] but returns the exit code too (an archive with no events fails `validate`).
pub fn import_raw(input: &Path, out: &Path) -> (i32, String) {
    cli(&[
        "import",
        input.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ])
}

/// Durable provenance of one event, as the `provenance` command reports it.
pub fn provenance(out: &Path, event_id: &str) -> String {
    serde_json::to_string(&convolith::report::provenance(out, event_id).unwrap()).unwrap()
}

pub fn assert_valid(out: &Path) {
    let (code, log) = cli(&["validate", out.to_str().unwrap()]);
    assert_eq!(code, 0, "{log}");
}

pub fn events(out: &Path) -> Vec<Event> {
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

/// `(format, status)` of every inventoried source whose path contains `needle`.
pub fn inventory(out: &Path, needle: &str) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("reports/SOURCE_INVENTORY.json")).unwrap(),
    )
    .unwrap();
    let rows = v.as_array().cloned().unwrap_or_else(|| {
        v.get("sources")
            .or_else(|| v.get("rows"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap()
    });
    rows.iter()
        .filter(|r| r["original_path"].as_str().unwrap_or("").contains(needle))
        .map(|r| {
            (
                r["format"].as_str().unwrap().to_string(),
                r["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

pub fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Build a zip holding `(member name, bytes)` entries, names used verbatim.
pub fn zip_of(path: &Path, members: &[(&str, Vec<u8>)]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in members {
        z.start_file(*name, opts).unwrap();
        z.write_all(bytes).unwrap();
    }
    z.finish().unwrap();
}

/// Run counters parsed from the CLI summary line, e.g. `events: 36 new, 0 duplicate`.
pub fn counts(log: &str, label: &str) -> u64 {
    let after = log
        .split(&format!("{label}: "))
        .nth(1)
        .unwrap_or_else(|| panic!("no {label} in {log}"));
    after
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// Path-traversal members must never be extracted or imported.
pub fn traversal_name() -> &'static str {
    "../escape/conversations.json"
}
