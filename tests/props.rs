//! Property tests (spec §55): archive path sanitization, timestamp parsing, the
//! JSONL line reader and canonical event JSON. `fuzz/` holds the cargo-fuzz
//! counterparts of the same properties.

use convolith::archive::sanitize_entry_path;
use convolith::model::{
    Event, EventType, Part, ProvenanceRef, ReasoningVisibility, Redaction, Role,
};
use convolith::parsers::{
    claude_code::ClaudeCodeParser, codex::CodexParser, generic::GenericJsonlParser,
};
use convolith::timeutil::{
    parse_epoch_like, parse_json_timestamp, parse_rfc3339, TimestampConfidence, Utc,
};
use proptest::prelude::*;
use serde_json::{json, Map, Value};
use std::path::{Component, Path};

#[path = "../fuzz/harness.rs"]
mod harness;

// ---- archive path sanitization ----------------------------------------------

/// Names built from the pieces that make traversal tricks: dots, both
/// separators, drive colons, NUL, reserved device names and plain text.
fn nasty_name() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("..".to_string()),
        Just(".".to_string()),
        Just("/".to_string()),
        Just("\\".to_string()),
        Just(":".to_string()),
        Just("\0".to_string()),
        Just("C:".to_string()),
        Just("CON".to_string()),
        Just("nul.txt".to_string()),
        Just("é".to_string()),
        "[a-zA-Z0-9_ -]{1,6}",
    ];
    prop::collection::vec(piece, 0..10).prop_map(|v| v.concat())
}

proptest! {
    #[test]
    fn sanitized_paths_never_escape_the_extraction_dir(name in prop_oneof![nasty_name(), any::<String>()]) {
        if let Some(p) = sanitize_entry_path(&name, 4096) {
            prop_assert!(p.is_relative());
            prop_assert!(p.components().all(|c| matches!(c, Component::Normal(_))));
            prop_assert!(Path::new("/stage").join(&p).starts_with("/stage"));
            let s = p.to_string_lossy();
            prop_assert!(!s.contains('\0'), "{s}");
            prop_assert!(!s.split(['/', '\\']).any(|c| c == ".."), "{s}");
            // Idempotent: a sanitized path sanitizes to itself.
            prop_assert_eq!(sanitize_entry_path(&s, 4096), Some(p.clone()));
        }
    }

    #[test]
    fn traversal_and_absolute_names_are_always_rejected(
        prefix in "[a-z]{0,5}", suffix in "[a-z]{0,5}", sep in prop_oneof![Just("/"), Just("\\")]
    ) {
        let dotdot = format!("{prefix}{sep}..{sep}{suffix}");
        prop_assert_eq!(sanitize_entry_path(&dotdot, 4096), None);
        let abs = format!("{sep}{prefix}{suffix}");
        prop_assert_eq!(sanitize_entry_path(&abs, 4096), None);
        let drive = format!("C:{sep}{prefix}");
        prop_assert_eq!(sanitize_entry_path(&drive, 4096), None);
    }
}

// ---- timestamp parsing ------------------------------------------------------

proptest! {
    #[test]
    fn rfc3339_roundtrips_for_every_valid_instant(secs in -9_223_372_035i64..9_223_372_035i64, nanos in 0i64..1_000_000_000) {
        // `Utc` is i64 nanoseconds: about years 1677..2262.
        let u = Utc(secs * 1_000_000_000 + nanos);
        let text = u.to_rfc3339();
        prop_assert_eq!(parse_rfc3339(&text), Some(u), "{}", text);
    }

    #[test]
    fn offsets_shift_the_instant_exactly(secs in 0i64..4_000_000_000i64, oh in 0i64..24, om in 0i64..60, neg in any::<bool>()) {
        let base = Utc(secs * 1_000_000_000);
        // Render the same instant as local time at a fixed offset.
        let off = (if neg { -1 } else { 1 }) * (oh * 3600 + om * 60);
        let local = Utc((secs + off) * 1_000_000_000).to_rfc3339();
        let local = local.trim_end_matches('Z');
        let sign = if neg { '-' } else { '+' };
        let text = format!("{local}{sign}{oh:02}:{om:02}");
        prop_assert_eq!(parse_rfc3339(&text), Some(base), "{}", text);
    }

    #[test]
    fn timestamp_parsers_never_panic_and_accepted_values_render(s in any::<String>(), n in any::<i64>(), f in any::<f64>()) {
        if let Some(u) = parse_rfc3339(&s) {
            prop_assert_eq!(parse_rfc3339(&u.to_rfc3339()), Some(u));
        }
        let _ = parse_json_timestamp(&Value::String(s));
        let _ = parse_json_timestamp(&json!(n));
        if let Some(num) = serde_json::Number::from_f64(f) {
            let _ = parse_json_timestamp(&Value::Number(num));
        }
        if let Some((u, conf)) = parse_epoch_like(n) {
            prop_assert_eq!(conf, TimestampConfidence::DatabaseDerived);
            let _ = u.to_rfc3339();
        }
    }

    #[test]
    fn out_of_range_years_are_rejected_not_wrapped(y in 0u32..10_000, mo in 1u32..13, d in 1u32..29) {
        // Every four-digit year is syntactically valid; those outside the i64
        // nanosecond range must come back `None`, never panic or wrap into a
        // different, wrong instant.
        let text = format!("{y:04}-{mo:02}-{d:02}T12:00:00Z");
        match parse_rfc3339(&text) {
            Some(u) => {
                prop_assert!((1677..=2262).contains(&y), "{text} accepted");
                prop_assert_eq!(u.to_rfc3339(), text);
            }
            None => prop_assert!(!(1678..=2261).contains(&y), "{text} rejected"),
        }
    }

    #[test]
    fn naive_local_times_are_never_guessed(y in 1970u32..2100, mo in 1u32..13, d in 1u32..29, h in 0u32..24) {
        let naive = format!("{y:04}-{mo:02}-{d:02}T{h:02}:00:00");
        prop_assert_eq!(parse_rfc3339(&naive), None, "no offset, no instant");
    }
}

// ---- JSONL line reader (through the parsers that use it) ----------------------

fn line() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..120),
        Just(br#"{"role":"user","content":"hello"}"#.to_vec()),
        Just(br#"{"type":"user","uuid":"u1","message":{"role":"user","content":"hi"}}"#.to_vec()),
        Just(br#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]}}"#.to_vec()),
        Just(b"   \t".to_vec()),
        Just(Vec::new()),
        (300usize..1500).prop_map(|n| vec![b'a'; n]),
        (300usize..1500).prop_map(|n| format!("{{\"k\":\"{}\"}}", "z".repeat(n)).into_bytes()),
    ]
}

fn file_bytes() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::collection::vec(line(), 0..12),
        prop_oneof![Just(&b"\n"[..]), Just(&b"\r\n"[..])],
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(lines, eol, bom, trailing)| {
            let mut out = if bom {
                vec![0xEF, 0xBB, 0xBF]
            } else {
                Vec::new()
            };
            for (i, l) in lines.iter().enumerate() {
                out.extend_from_slice(l);
                if i + 1 < lines.len() || trailing {
                    out.extend_from_slice(eol);
                }
            }
            out
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn every_non_blank_line_is_exactly_one_outcome(bytes in file_bytes()) {
        let parsers: [&dyn convolith::parser::SourceParser; 3] =
            [&ClaudeCodeParser, &CodexParser, &GenericJsonlParser];
        for p in parsers {
            // A whole-source error is allowed; a panic or a lost line is not.
            if let Ok((events, r)) = harness::parse_bytes(p, &bytes, 256) {
                let lines = harness::non_blank_lines(&bytes);
                // The generic parser leaves `records_examined` to the importer.
                if p.id() != "generic_jsonl" {
                    prop_assert_eq!(r.records_examined, lines, "{}", p.id());
                }
                prop_assert_eq!(events as u64 + r.records_skipped + r.records_failed, lines, "{}", p.id());
            }
        }
    }
}

// ---- canonical event JSON -----------------------------------------------------

/// JSON without floats (their text form is not canonical across encoders) and
/// with bounded depth.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(any::<String>(), inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

fn opt_s() -> impl Strategy<Value = Option<String>> {
    prop::option::of(any::<String>())
}

fn part() -> impl Strategy<Value = Part> {
    prop_oneof![
        any::<String>().prop_map(Part::text),
        (
            any::<String>(),
            prop_oneof![
                Just(ReasoningVisibility::Public),
                Just(ReasoningVisibility::Summary)
            ]
        )
            .prop_map(|(text, visibility)| Part::Reasoning { text, visibility }),
        (opt_s(), any::<String>(), json_value()).prop_map(|(id, name, arguments)| Part::ToolCall {
            id,
            name,
            arguments
        }),
        (opt_s(), json_value(), any::<bool>()).prop_map(|(tool_call_id, output, is_error)| {
            Part::ToolResult {
                tool_call_id,
                output,
                is_error,
            }
        }),
        (any::<String>(), opt_s(), prop::option::of(json_value()))
            .prop_map(|(kind, note, raw)| Part::Opaque { kind, note, raw }),
        json_value().prop_map(|value| Part::Data { value }),
        (any::<String>(), opt_s(), opt_s(), any::<u64>()).prop_map(
            |(artifact, mime, filename, size)| Part::Artifact {
                artifact,
                mime,
                filename,
                size
            }
        ),
    ]
}

fn event() -> impl Strategy<Value = Event> {
    let ids = (
        any::<String>(),
        any::<String>(),
        opt_s(),
        opt_s(),
        any::<u64>(),
    );
    let meta = (
        opt_s(),
        opt_s(),
        prop_oneof![
            Just(TimestampConfidence::Exact),
            Just(TimestampConfidence::Unknown),
            Just(TimestampConfidence::SequenceOnly)
        ],
        prop_oneof![
            Just(Role::User),
            Just(Role::Assistant),
            Just(Role::Tool),
            Just(Role::Other)
        ],
        prop_oneof![
            Just(EventType::Message),
            Just(EventType::ToolCall),
            Just(EventType::ToolResult),
            Just(EventType::Opaque)
        ],
    );
    let ctx = (
        any::<String>(),
        any::<String>(),
        opt_s(),
        opt_s(),
        opt_s(),
        opt_s(),
    );
    let body = (
        prop::collection::vec(part(), 0..4),
        prop::collection::btree_map(any::<String>(), json_value(), 0..3),
        prop::collection::vec((any::<String>(), any::<u32>(), any::<String>()), 0..3),
        prop::collection::vec(
            (
                any::<String>(),
                prop::collection::vec(any::<String>(), 0..3),
                prop::option::of(any::<u64>()),
            ),
            0..3,
        ),
    );
    (ids, meta, ctx, body).prop_map(
        |(
            (event_id, conversation_id, session_id, parent_event_id, seq),
            (timestamp, timestamp_original, timestamp_confidence, role, event_type),
            (provider, application, model, agent, working_directory, branch),
            (content, metadata, redactions, prov),
        )| Event {
            schema_version: 1,
            event_id,
            conversation_id,
            session_id,
            parent_event_id,
            seq,
            timestamp,
            timestamp_original,
            timestamp_confidence,
            role,
            event_type,
            provider,
            application,
            model,
            agent,
            machine_id: None,
            project_id: None,
            repository_id: None,
            working_directory,
            branch,
            commit: None,
            worktree_id: None,
            content,
            metadata: metadata.into_iter().collect(),
            redactions: redactions
                .into_iter()
                .map(|(kind, count, field)| Redaction { kind, count, field })
                .collect(),
            provenance: prov
                .into_iter()
                .map(|(p, chain, idx)| ProvenanceRef {
                    source_id: p.clone(),
                    source_path: p,
                    container_chain: chain,
                    record_index: idx,
                    record_id: None,
                    parser: "p".into(),
                    parser_version: "1".into(),
                    source_sha256: None,
                    first_seen: "2026-01-01T00:00:00Z".into(),
                    import_run: "run".into(),
                    identity_tier: "native".into(),
                })
                .collect(),
        },
    )
}

proptest! {
    #[test]
    fn canonical_event_json_roundtrips_and_is_stable(e in event()) {
        let a = serde_json::to_string(&e).unwrap();
        prop_assert!(!a.contains('\n'), "one record, one line");
        let back: Event = serde_json::from_str(&a).unwrap();
        prop_assert_eq!(&back, &e);
        prop_assert_eq!(serde_json::to_string(&back).unwrap(), a);
    }

    #[test]
    fn garbage_never_deserializes_into_a_panic(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let _ = serde_json::from_slice::<Event>(&bytes);
    }
}
