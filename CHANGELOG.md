# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The canonical record shape carries its own `schema_version` (currently 1), which
is independent of the crate version.

## [Unreleased]

### Added

- Parsers for official web-chat exports, checked against real exports: Claude
  (`claude_web_export`: split data export with `manifest-*.json`; real retry/edit branches
  via `parent_message_uuid`), Claude Design chats (`claude_design_export`), Perplexity
  (`perplexity_export`: `user_data_export_*.zip`) and Gemini Google Takeout
  (`gemini_takeout`: Conversation History; each array element is a user *or* model turn
  and `turn_index` may repeat). ChatGPT (existing parser, extended: sharded
  `conversations-NNN.json`, current-path flag, asset-member names, richer metadata) is now
  verified against a real export too. Unverified-against-real-export: Gemini My Activity JSON. Zips, extracted folders and bare files auto-detect.
  Unrecognised schemas and the other files of these exports (manifests, memories, projects,
  feedback, design frames, account metadata/workbooks, NotebookLM, images, Office parts)
  are inventoried as unsupported with a reason. Web-export events carry the importing
  machine id.

- `convolith all ARCHIVE (--output FILE | --stdout)`: read-only export of every canonical
  event as one globally chronological, deduplicated, deterministic JSONL stream in the
  provider-independent `convolith.all/v1` schema (`spec/all-export.md`,
  `spec/all-export.schema.json`). Events without a usable timestamp keep
  `timestamp: null` and are placed after all timestamped events.

### Changed

- ChatGPT export parsing keeps the v0.1.0 event and conversation ids and content
  fingerprints (node keys as event ids, `id` as conversation id, v0.1.0 parts), so
  re-importing a ChatGPT export into an archive made by v0.1.0 creates no new events or
  conversations; richer data (`metadata.attachments`, non-text content, current-path
  flag, `update_time`) is added to event metadata only. Locked by a golden-id test and
  confirmed identical to the v0.1.0 binary on a real 84-conversation export.
- Project renamed from `aichive` to `Convolith` (crate, binary, lib, `CONVOLITH_*` env,
  docs). New manifests record `tool: "convolith"`. The `convolith-canonical` format id,
  the `convolith-local-machine-v1` machine-id hash input, the `.convolith-complete`
  staging marker and the `convolith.example` schema `$id` URIs were renamed too.
  Existing archives stay valid: the legacy `aichive-canonical` format id is still
  accepted (and upgraded on the next append) and the legacy `.aichive-complete` marker is
  still honoured on `--resume`. Local machine ids hashed under the old input differ from
  new ones, but archives holding them (or `linux`/`windows` ids) validate unchanged.

## [1.0.0] - v1 integration

### Added

- `minimax` parser (MiniMax Code, `~/.minimax/v2/sessions/**/messages.jsonl` and
  `snapshots/g*--ctx_*.jsonl`) and `deepseek` parser (DeepSeek DSH,
  `~/.dsh/sessions/**/session.v4.jsonl`, zstd members expanded by discovery). Files
  elsewhere in those stores are inventoried as unsupported with a reason
  (`parsers::known_unsupported`), including SQLite `-wal`/`-shm`/`-journal` sidecars.
  Real local data: 7,505 MiniMax events / 23 conversations (previously all of
  `~/.minimax`, ~5.7k files, was unknown) and 1,229 DeepSeek events / 2 conversations
  (Windows-side `.dsh`, via `convolith import`).

### Changed

- Remote (WSL/SSH) collection no longer copies whole store directories. The store
  table carries per-store file rules (`Wanted`); `path_wanted` applies them locally and
  the same patterns generate the remote `find` listing, then `tar -cf - -T -` archives
  exactly the files that pass the shared filter (plus SQLite companions and siblings
  such as Gemini `.project_root`). Provenance is unchanged. Hermes collection is narrowed
  to `state.db` and `sessions/`; the unfounded `.mcode` and `.deepseek` store entries
  were replaced by `.dsh`; `~/.minimax` is walked only for session files.
- Previous Gemini/Antigravity narrowing is now one case of those rules.

- `collect` (and `discover --local`) walk only known Gemini/Antigravity inputs under
  `~/.gemini`: `antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl`,
  `tmp/*/chats/session-*.{jsonl,json}` and the sibling `tmp/*/.project_root`. Agent
  work files, chunks, logs and protobuf stores are no longer enumerated (a real
  `~/.gemini`: 36k files, 64s -> 10s). The imported event set is identical (26,749
  event ids before and after). Tradeoff: unsupported-file inventory counts shrink for
  files no longer walked; `convolith import PATH` is unchanged and still inventories
  everything it is given.

### Fixed

- Fuzz harness counted a lone VT byte as a blank line while `LineReader` (correctly)
  reports it as a failing record; surfaced by a proptest regression.
- `Ledger::rebuild_aggregates` referenced nonexistent `event_index.working_directory`
  and `branch`; `project_path` now comes from `conversation`, and `project_branch` /
  `machine_evidence` (import-time facts) are no longer wiped. Regression test added.
- `sanitize_entry_path` accepted `./C:` (drive-relative on Windows) and was not
  idempotent; leading `./` is stripped before the drive check.

### Known limitations

See README "Known limitations".

## [Unreleased]

### Added

- `gemini_cli` and `antigravity` parsers (provider `google`). Gemini CLI JSONL/JSON
  sessions (rewritten message ids folded last-wins, thoughts, tool calls + results,
  token usage, `.project_root` working directory) and Antigravity
  `transcript_full.jsonl` steps (user, planner, tool output, system, errors;
  `step_index` is metadata, identity is position + content). Files under `~/.gemini`
  that cannot be read faithfully (protobuf SQLite trajectories, `.pb`/`.pbtxt`) or
  would duplicate imported events (`transcript.jsonl`, chunks, prompt logs) are
  inventoried as unsupported with a per-format reason. `discover` now also knows
  `~/.gemini/antigravity-cli`. No schema change.
- `convolith collect [--local] [--wsl] [--ssh USER@HOST]... [--all-machines] [--apps a,b]
  [--dry-run] [--deep] [--resume]`: finds known stores (one app -> path table in
  `discover.rs`, shared by every machine kind) on this host, each WSL distro
  (`wsl.exe -l -q`, discovery runs inside the distro) and SSH hosts (system `ssh`,
  a temporary `sh` script over stdin, tar back), then imports through the normal
  pipeline. Provenance carries `machine_id` (`windows`, `wsl/example-distro`, `ssh/ssh-host.example`)
  and the original remote path. A failing machine never aborts the others;
  `collect-state.json` lets `--resume` skip unchanged stores; a malformed state file is an error. No schema change.
- `convolith discover --local` walks known per-user stores (Windows `%USERPROFILE%`,
  `%APPDATA%`, `%LOCALAPPDATA%`; Linux/WSL `$HOME`, `$XDG_*`, `$CODEX_HOME`,
  `$CLAUDE_CONFIG_DIR`), skipping browser caches.
- LevelDB (Electron profile) stores are detected, inspected on a staged copy and
  inventoried as unresolved; never decoded into conversations without a decoder.
- Tool outputs past `limits.max_inline_text_bytes` spill to the artifact store,
  referenced from the event; artifacts now list their related event ids and
  `data/aggregates/artifacts.jsonl.zst` is written.
- Property tests (`tests/props.rs`) and a `fuzz/` cargo-fuzz crate.

### Fixed

- Unix paths are no longer case-folded and path prefixes (machine/project
  aliases) match on component boundaries; UNC and `\\?\` paths normalize.
- Private-key redaction covers the whole PEM block, not just its header; the
  privacy audit counts each secret kind separately.
- Unsupported sources are recorded in the ledger so the reports list them.
- RFC 3339 parsing rejects impossible dates (`02-30`) and years outside the
  nanosecond range instead of overflowing.

## [0.1.0] - 2025-01-01

First cut. Interfaces and the canonical format are still moving before 1.0.

### Added

- `convolith` CLI: `discover`, `inspect`, `import`, `validate`, `stats`, `search`,
  `report`, `rebuild-index`, `provenance`, `inspect-event`, `parsers`.
- Canonical dataset layout (`manifest.json`, `data/events/part-NNNNNN.jsonl.zst`,
  `data/aggregates/*.jsonl.zst`, `artifacts/sha256/**`, `indexes/search.sqlite`,
  `provenance/provenance.sqlite`, `reports/**`, `checksums.sha256`).
- Canonical event model (`schema_version` 1): roles, event types, twelve content
  part types, reasoning visibility, truncation references, per-event redactions,
  in-band provenance observations.
- Deterministic BLAKE3-derived identifiers for events, conversations, sessions,
  projects, repositories, sources, artifacts and import runs; SHA-256 for stored
  bytes and `checksums.sha256`.
- Three-tier deduplication (native id, coordinates, content fingerprint) with
  idempotent observations, and conflict preservation: same identity with
  different content keeps both variants as separate events linked by
  `variant_of`.
- Provenance ledger (SQLite) with source inventory, source-level record
  accounting, an idempotent observation index, conflicts, parse errors and
  regenerable aggregates.
- Parsers: Claude Code session JSONL (incl. subagent sidechains, compaction,
  summaries, attachments as opaque parts), Codex rollout JSONL (incl. legacy
  header shape, reasoning summaries, encrypted reasoning kept opaque), ChatGPT
  export `conversations.json` (mapping graph with forks preserved), and generic
  JSON / JSON Lines fallbacks.
- Bounded, read-only discovery and import: archive expansion (zip/tar/gzip/zstd)
  into staging with entry-name sanitization and per-archive limits, rollback-journal
  and WAL-replay handling for SQLite sources on a staged copy, file/record/artifact/
  staging/file-count/depth limits.
- Secret detection and redaction policy (`redact` default, `preserve` optional)
  applied to content, tool arguments and tool output, recorded per event and
  aggregated in `reports/PRIVACY_AUDIT.md` with counts only.
- Reporting: import report, source coverage, duplicates, conflicts, parse errors,
  data quality, privacy audit, source inventory (Markdown + JSON).
- Import reports, source inventory, FTS search index and content-addressed
  artifacts.
- `--resume` (skip sources whose size/mtime/parser fingerprint is unchanged and
  whose last run completed) and `--dry-run`.
- Validation: manifest, layout, checksums, event ids, ledger agreement,
  provenance coverage, record accounting and referential integrity; exit 1 on any
  `FAIL`.
- Public spec: [`spec/README.md`](spec/README.md) plus JSON Schema 2020-12 for
  event, conversation, artifact and provenance, with validated examples.
- Parsers for opencode (file store, `opencode.db`, export bundles), Pi and
  Oh My Pi JSONL tree sessions (all branches, not just the active path), and
  Hermes Agent (state database, session JSON logs, gateway transcripts);
  opencode/pi/hermes unknown part kinds are kept opaque rather than dropped.
- Acceptance tests over synthetic fixtures, including overlap/backup collapse,
  export-vs-store non-merge, fork handling, truncated inputs, oversize records and
  resume behaviour.

### Known limitations

- Windows is built and tested in CI only, not runtime-tested on a real machine;
  macOS is untested.
- opencode, pi/Oh My Pi and hermes have parsers and acceptance coverage but no
  real-world corpus testing; their formats are private and undocumented.
- Other sources (Gemini CLI, Cursor, Aider, Continue, Cline, SQLite chat stores,
  …) are not implemented.
- Redaction is best-effort pattern matching, not sanitization.
- Parsers have not been fuzzed continuously.

[Unreleased]: https://github.com/example/convolith/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/example/convolith/releases/tag/v0.1.0
# Unreleased

- Add deterministic `convolith pack` / `unpack` portable archive containers.
