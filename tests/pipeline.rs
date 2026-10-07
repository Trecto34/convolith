//! End-to-end pipeline test: import a tiny fixture, then exercise the three
//! library modules the CLI drives — `report`, `search` and `validate`.
//!
//! The fixture uses a local `SourceParser` because no real parser ships with the
//! library yet; everything else is the production path.

use anyhow::Result;
use convolith::dataset::{self, Layout, ShardRef};
use convolith::importer::{ImportOptions, Importer};
use convolith::model::{EventDraft, Part, Role};
use convolith::parser::{
    ConversationMeta, EventSink, IdentityHint, ParseContext, Registry, SourceParser,
};
use convolith::report;
use convolith::search;
use convolith::secrets::SecretPolicy;
use convolith::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use convolith::validate;
use std::io::Write;
use std::path::{Path, PathBuf};

const SECRET: &str = "sk-abcdefghijklmnopqrstuvwxyz012345";

struct LineParser;

impl SourceParser for LineParser {
    fn id(&self) -> &'static str {
        "test_lines"
    }
    fn provider(&self) -> &'static str {
        "acme"
    }
    fn application(&self) -> &'static str {
        "acme-chat"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: false,
            attachments: false,
            reasoning: false,
            streaming: true,
            partial: false,
        }
    }
    fn description(&self) -> &'static str {
        "one JSON object per line"
    }
    fn detect(&self, probe: &Probe) -> Detection {
        if probe.ext() == "jsonl" {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "jsonl",
                Confidence::Strong,
                "jsonl extension",
            )
        } else {
            Detection::none(self.id())
        }
    }
    fn parse(
        &self,
        _ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport> {
        sink.begin(ConversationMeta {
            provider: Some(self.provider().into()),
            application: Some(self.application().into()),
            native_id: Some("conv-1".into()),
            native_session_id: Some("sess-1".into()),
            title: Some("fixture conversation".into()),
            identity_hint: IdentityHint::Native,
            ..Default::default()
        })?;
        let text = std::fs::read_to_string(&source.read_path)?;
        let mut report = ParseReport::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line)?;
            report.records_examined += 1;
            report.events += 1;
            let mut draft = EventDraft::with_content(
                Role::User,
                convolith::model::EventType::Message,
                vec![Part::text(v["text"].as_str().unwrap_or_default())],
            );
            draft.native_id = Some(v["id"].as_str().unwrap_or_default().to_string());
            draft.timestamp =
                convolith::timeutil::parse_rfc3339(v["ts"].as_str().unwrap_or_default())
                    .map(|utc| {
                        convolith::timeutil::Stamp::from_utc(
                            utc,
                            convolith::timeutil::TimestampConfidence::Exact,
                            None,
                        )
                    })
                    .unwrap_or_else(convolith::timeutil::Stamp::unknown);
            sink.emit(draft)?;
        }
        sink.end()?;
        Ok(report)
    }
}

fn fixture(root: &Path) -> Result<()> {
    let store = root.join("store");
    std::fs::create_dir_all(&store)?;
    let mut f = std::fs::File::create(store.join("chat.jsonl"))?;
    writeln!(
        f,
        r#"{{"id":"m1","ts":"2024-01-02T03:04:05Z","text":"the quick brown fox jumps"}}"#
    )?;
    writeln!(
        f,
        r#"{{"id":"m2","ts":"2024-01-02T03:05:06Z","text":"rotary telephone maintenance"}}"#
    )?;
    writeln!(
        f,
        r#"{{"id":"m3","ts":"2024-01-02T03:06:07Z","text":"here is the key {SECRET} be careful"}}"#
    )?;
    Ok(())
}

/// Files the caller owns and that must stay out of `checksums.sha256`.
fn derived_skip(ds: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for sub in ["indexes", "staging"] {
        let dir = ds.join(sub);
        if dir.is_dir() {
            for e in walkdir::WalkDir::new(&dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if e.file_type().is_file() {
                    out.push(e.path().to_path_buf());
                }
            }
        }
    }
    out
}

/// Build a dataset, then finalize it the way the CLI does: reports, checksums,
/// index. Returns the dataset root.
fn build_dataset(tmp: &Path) -> Result<PathBuf> {
    let src = tmp.join("input");
    fixture(&src)?;
    let ds = tmp.join("canonical");
    let opts = ImportOptions {
        output: ds.clone(),
        secrets: SecretPolicy::Redact,
        machine_id: Some("machine-1".into()),
        platform: Some("linux".into()),
        args: vec!["import".into(), src.display().to_string()],
        ..Default::default()
    };
    let mut importer = Importer::new(
        opts,
        convolith::config::Config::default(),
        Registry::new(vec![Box::new(LineParser)]),
    )?;
    let stats = importer.import(&[src])?;
    assert_eq!(stats.events_new, 3, "three events imported");
    assert!(
        stats.accounting_holds(),
        "import accounting identity: {stats:?}"
    );

    report::generate(&ds)?;
    search::rebuild_index(&ds)?;
    dataset::write_checksums(&ds, &derived_skip(&ds))?;
    Ok(ds)
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("convolith-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn import_then_report_writes_every_deliverable_with_real_counts() -> Result<()> {
    let tmp = tmpdir("report");
    let ds = build_dataset(&tmp)?;

    let expected = [
        "IMPORT_REPORT.md",
        "SOURCE_COVERAGE.md",
        "DUPLICATES.md",
        "CONFLICTS.md",
        "PARSE_ERRORS.md",
        "DATA_QUALITY.md",
        "PRIVACY_AUDIT.md",
        "SOURCE_INVENTORY.json",
        "SOURCE_INVENTORY.md",
    ];
    for name in expected {
        let p = ds.join("reports").join(name);
        assert!(p.is_file(), "missing report {name}");
        assert!(std::fs::metadata(&p)?.len() > 0, "empty report {name}");
    }

    let stats = report::stats(&ds)?;
    assert_eq!(stats.events_in_shards, 3);
    assert_eq!(stats.events_indexed, 3);
    assert_eq!(stats.sources, 1);
    assert_eq!(stats.conversations, 1);
    assert_eq!(stats.sessions, 1);
    assert_eq!(stats.observations, 3);
    assert_eq!(stats.events_redacted, 1);
    assert_eq!(stats.redaction_kinds.get("openai_key"), Some(&1));
    assert!(stats.accounting_holds(), "{stats:?}");
    assert!(stats.ledger_in_sync(), "{stats:?}");

    let privacy = std::fs::read_to_string(ds.join("reports").join("PRIVACY_AUDIT.md"))?;
    assert!(privacy.contains("openai_key"), "{privacy}");
    let inventory = std::fs::read_to_string(ds.join("reports").join("SOURCE_INVENTORY.md"))?;
    assert!(inventory.contains("chat.jsonl"), "{inventory}");

    // No report may carry a conversation body or a secret value.
    for name in expected {
        let text = std::fs::read_to_string(ds.join("reports").join(name))?;
        assert!(!text.contains(SECRET), "{name} leaked the secret");
        assert!(
            !text.contains("quick brown fox"),
            "{name} carries a conversation body"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn search_finds_imported_events_and_never_surfaces_a_secret() -> Result<()> {
    let tmp = tmpdir("search");
    let ds = build_dataset(&tmp)?;

    let hits = search::search(&ds, "quick", 10)?;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].provider, "acme");
    assert_eq!(hits[0].application, "acme-chat");
    assert_eq!(hits[0].timestamp.as_deref(), Some("2024-01-02T03:04:05Z"));
    assert!(hits[0].preview.contains("quick"), "{hits:?}");
    assert!(hits[0].event_id.starts_with("ev_"));

    // All terms must match, and a term that is not there finds nothing.
    assert_eq!(search::search(&ds, "quick brown", 10)?.len(), 1);
    assert!(search::search(&ds, "quick zebra", 10)?.is_empty());
    assert!(search::search(&ds, "   ", 10)?.is_empty());

    // FTS5 syntax in the query is neutralised, not executed.
    assert!(search::search(&ds, "NEAR(\"fox", 10)?.is_empty());

    // The redacted body is indexed; the raw key is not, anywhere.
    assert_eq!(search::search(&ds, "careful", 10)?.len(), 1);
    assert!(
        search::search(&ds, SECRET, 10)?.is_empty(),
        "raw secret matched the index"
    );
    let raw = std::fs::read(ds.join("indexes").join("search.sqlite"))?;
    let needle = SECRET.as_bytes();
    assert!(
        !raw.windows(needle.len()).any(|w| w == needle),
        "secret bytes present in the index file"
    );

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn index_is_derived_and_rebuilds_from_scratch() -> Result<()> {
    let tmp = tmpdir("rebuild");
    let ds = build_dataset(&tmp)?;

    let before = search::search(&ds, "rotary", 10)?;
    assert_eq!(before.len(), 1);

    std::fs::remove_file(search::index_path(&ds))?;
    // A missing index is an error the operator can act on, not an empty result.
    let err = search::search(&ds, "rotary", 10).unwrap_err().to_string();
    assert!(err.contains("rebuild_index"), "{err}");

    let stats = search::rebuild_index(&ds)?;
    assert_eq!(stats.events_indexed, 3);
    // The index covers every `.jsonl.zst` stream; only the event shard yields
    // rows, so the counts differ by design.
    assert!(stats.shards >= 1, "{stats:?}");
    assert!(stats.skipped_records >= 1, "{stats:?}");
    let after = search::search(&ds, "rotary", 10)?;
    assert_eq!(after.len(), 1);
    assert_eq!(before, after, "a rebuild is not allowed to change results");

    // Deleting the index never harms the dataset itself.
    std::fs::remove_dir_all(ds.join("indexes"))?;
    assert_eq!(report::stats(&ds)?.events_in_shards, 3);

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn provenance_and_inspect_event_resolve_real_records() -> Result<()> {
    let tmp = tmpdir("prov");
    let ds = build_dataset(&tmp)?;

    let hit = search::search(&ds, "rotary", 10)?.remove(0);
    let rows = report::provenance(&ds, &hit.event_id)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].parser, "test_lines");
    assert!(rows[0].source_path.contains("chat.jsonl"), "{rows:?}");

    let event = report::inspect_event(&ds, &hit.event_id)?;
    assert_eq!(event.event_id, hit.event_id);
    assert_eq!(event.conversation_id, hit.conversation);
    assert_eq!(event.session_id.as_deref(), Some("sess-1"));

    // Unknown ids are errors, not empty results.
    assert!(report::provenance(&ds, "ev_nope").is_err());
    assert!(report::inspect_event(&ds, "ev_nope").is_err());

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn validate_passes_a_clean_dataset() -> Result<()> {
    let tmp = tmpdir("valid-ok");
    let ds = build_dataset(&tmp)?;

    let report = validate::validate(&ds)?;
    assert!(report.passed(), "{}", report.summary());
    for name in [
        "layout",
        "manifest",
        "checksums",
        "provenance",
        "event_ids",
        "accounting",
        "referential",
    ] {
        assert!(report.is_pass(name), "check {name} failed: {report:?}");
    }
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert_eq!(report.check("event_ids").unwrap().status.as_str(), "PASS");

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn writes_new_format_id_and_accepts_the_legacy_one() -> Result<()> {
    let tmp = tmpdir("valid-format");
    let ds = build_dataset(&tmp)?;
    let path = ds.join("manifest.json");
    let text = std::fs::read_to_string(&path)?;
    assert!(
        text.contains("\"format\": \"convolith-canonical\""),
        "{text}"
    );

    std::fs::write(
        &path,
        text.replace("convolith-canonical", "aichive-canonical"),
    )?;
    let report = validate::validate(&ds)?;
    assert!(report.is_pass("manifest"), "{}", report.summary());

    std::fs::write(
        &path,
        text.replace("convolith-canonical", "other-canonical"),
    )?;
    assert!(!validate::validate(&ds)?.is_pass("manifest"));

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn durable_provenance_survives_derived_data_rebuild_and_is_required_for_archive() -> Result<()> {
    let tmp = tmpdir("provenance-durability");
    let ds = build_dataset(&tmp)?;
    let layout = Layout { root: ds.clone() };
    let ledger_rel = "provenance/provenance.sqlite";
    let checksums = std::fs::read_to_string(layout.checksums())?;
    assert!(checksums.lines().any(|line| line.ends_with(ledger_rel)));

    // Get a real id through the ledger-backed search index.
    let hit = search::search(&ds, "rotary", 10)?.remove(0);
    let event = report::inspect_event(&ds, &hit.event_id)?;
    assert!(
        event.provenance.is_empty(),
        "current imports keep observations in the ledger"
    );

    // A copy of just manifest + event shards retains events but cannot retain
    // source refs, machine/path/provider data or satisfy dataset validation.
    let minimal = tmp.join("canonical-only");
    std::fs::create_dir_all(minimal.join("data/events"))?;
    std::fs::copy(layout.manifest(), minimal.join("manifest.json"))?;
    for shard in std::fs::read_dir(layout.events_dir())? {
        let shard = shard?;
        std::fs::copy(
            shard.path(),
            minimal.join("data/events").join(shard.file_name()),
        )?;
    }
    let minimal_validation = validate::validate(&minimal)?;
    assert!(!minimal_validation.passed());
    assert!(!minimal_validation.is_pass("provenance"));

    // Remove the regenerable search index and reports. The ledger observation
    // remains the source of truth for the source path and machine identity.
    for dir in [ds.join("indexes"), ds.join("reports")] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
    }
    search::rebuild_index(&ds)?;
    report::generate(&ds)?;
    dataset::write_checksums(&ds, &derived_skip(&ds))?;

    let rows = report::provenance(&ds, &hit.event_id)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].parser, "test_lines");
    assert!(rows[0].source_path.contains("chat.jsonl"));
    assert_eq!(rows[0].machine_id.as_deref(), Some("machine-1"));
    assert_eq!(event.provider, "acme");
    assert_eq!(event.session_id.as_deref(), Some("sess-1"));
    let validation = validate::validate(&ds)?;
    assert!(validation.passed(), "{}", validation.summary());

    let checksum_path = layout.checksums();
    let checksums = std::fs::read_to_string(&checksum_path)?;
    std::fs::write(
        &checksum_path,
        checksums
            .lines()
            .filter(|line| !line.ends_with(ledger_rel))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )?;
    let validation = validate::validate(&ds)?;
    assert!(!validation.is_pass("checksums"));
    assert!(validation
        .issues
        .iter()
        .any(|issue| issue.contains("durable provenance ledger not listed")));

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn validate_fails_on_tampering_with_reasons() -> Result<()> {
    let tmp = tmpdir("valid-bad");
    let ds = build_dataset(&tmp)?;

    // Forge a second shard holding a re-used event id and an event the ledger
    // never observed: the shard is in neither the manifest nor the checksums.
    let existing = report::inspect_event(&ds, &search::search(&ds, "rotary", 10)?[0].event_id)?;
    let mut duplicated = existing.clone();
    duplicated.provenance.clear();
    let mut unseen = existing.clone();
    unseen.event_id = "ev_forged_unseen".into();
    unseen.provenance.clear();
    let shard_rel = "data/events/part-999999.jsonl.zst";
    let shard_path = ds.join(shard_rel);
    let mut w = dataset::JsonlZstWriter::create(&shard_path, 3)?;
    w.write_record(&duplicated)?;
    w.write_record(&unseen)?;
    let written: ShardRef = w.finish(shard_rel, "forged")?;
    assert!(written.records == 2);

    let report = validate::validate(&ds)?;
    assert!(!report.passed());
    let failed: Vec<&str> = report.failures().iter().map(|c| c.name.as_str()).collect();
    for name in ["checksums", "event_ids", "provenance"] {
        assert!(
            failed.contains(&name),
            "expected {name} to fail: {failed:?}"
        );
    }
    let joined = report.issues.join("\n");
    assert!(joined.contains("not listed in checksums"), "{joined}");
    assert!(joined.contains("duplicate event id"), "{joined}");
    assert!(joined.contains("without provenance"), "{joined}");
    // The tampered shard is introduced after the checksums were written, so the
    // manifest's record total no longer matches what is on disk.
    assert!(
        !report.is_pass("manifest_shards"),
        "manifest shard totals should disagree: {report:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[test]
fn validate_reports_a_missing_manifest_as_failure_not_error() -> Result<()> {
    let tmp = tmpdir("valid-nomanifest");
    let ds = build_dataset(&tmp)?;
    std::fs::remove_file(Layout { root: ds.clone() }.manifest())?;

    let report = validate::validate(&ds)?;
    assert!(!report.passed());
    assert!(!report.is_pass("manifest"));
    assert!(report.summary().contains("FAIL"));

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}
