#![no_main]
//! The JSONL line reader, reached through the line-oriented parsers: arbitrary
//! bytes never panic, and every non-blank line ends as exactly one event, skip
//! or failure.
use convolith::parsers::{claude_code::ClaudeCodeParser, codex::CodexParser, generic::GenericJsonlParser};
use libfuzzer_sys::fuzz_target;

#[path = "../harness.rs"]
mod harness;

fuzz_target!(|data: &[u8]| {
    let parsers: [&dyn convolith::parser::SourceParser; 3] =
        [&ClaudeCodeParser, &CodexParser, &GenericJsonlParser];
    for p in parsers {
        if let Ok((events, r)) = harness::parse_bytes(p, data, 256) {
            let lines = harness::non_blank_lines(data);
            // The generic parser leaves `records_examined` to the importer.
            if p.id() != "generic_jsonl" {
                assert_eq!(r.records_examined, lines);
            }
            assert_eq!(events as u64 + r.records_skipped + r.records_failed, lines);
        }
    }
});
