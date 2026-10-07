# Contributing

Thanks for helping. Two things matter more than anything else here:

1. **Never lose history.** A missing record is worse than a duplicated one.
2. **Never claim what you did not verify.** If a parser drops something, count
   and report it. If a platform is untested, say so.

## Build and test

```sh
cargo build
cargo test                 # unit tests + tests/{acceptance,pipeline,review2,parsers_claude_codex}.rs
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Requires Rust 1.75+ (edition 2021). There are no `dev-dependencies`: tests use
the std harness and the public library API only. CI runs fmt + clippy + test on
`ubuntu-latest` and `windows-latest`.

Extra checks that are not in CI (they are occasional, not per-commit):

```sh
sh spec/validate-examples.sh          # examples against spec/*.schema.json (ajv or python jsonschema)
convolith import fixtures/claude-code -o /tmp/ds && convolith validate /tmp/ds
```

## What goes in which directory

| Path | Rule |
|------|------|
| `src/model.rs` | the canonical record shape; changing it means changing `spec/` in the same PR |
| `src/parsers/<name>.rs` | one source format; never reach into the importer |
| `src/dataset.rs`, `src/ledger.rs`, `src/artifacts.rs` | layout, ledger and content-addressed storage |
| `spec/` | public format spec + JSON Schemas + validated examples |
| `docs/` | prior art and per-parser mapping notes |
| `fixtures/<source>/` | synthetic inputs only — **never** real session content |
| `tests/` | acceptance tests that drive the CLI/library end to end |

## Parser contributor guide

A parser's job: turn one source format into canonical events, without deciding
anything the importer should decide.

### 1. Implement the trait

In `src/parsers/<name>.rs`, implement
[`SourceParser`](../src/parser.rs): `id`, `provider`, `application`,
`description`, `capabilities`, `detect`, `parse`.

- Report `Capabilities` **honestly**. `attachments: false` means you do not
  decode attachments; `partial: true` means you knowingly skip record kinds (and
  therefore must count them).
- `detect` reads only what `Probe` already loaded (file extension, parent
  component, sibling names, bounded head bytes). Never open the file again, and
  never read it whole just to decide.
- `parse` streams: call `sink.begin(meta)`, then `sink.emit(draft)` per event,
  then `sink.end()`. Do not buffer the whole source; a 200 MB rollout must not
  become 200 MB of RAM.

### 2. Fill `ConversationMeta` truthfully

- `native_id`: the provider's own conversation id, **only if it really is
  globally unique** (a UUID or a long opaque token). This is what lets the same
  conversation collapse across snapshots. If you put `"msg_1"` or a filename
  here, unrelated conversations will merge.
- `identity_hint`: `Native` only when the records carry provider-issued globally
  unique ids. Otherwise leave the `Fingerprint` default — the conservative tier
  that can never over-merge.
- `working_directory`, `git_remote`, `branch` when the source names them. Never
  guess a project from a path that is not in the record.

### 3. Emit drafts, not ids

`EventDraft` is the input; the importer derives `event_id`, the identity tier,
`content_fp`, conversational `seq`, project/repository ids and the secret policy
application. Set:

- `native_id` / `parent_native_id` from the *source record* id and its parent.
  Keep the tree shape verbatim: forks and retries are siblings, not a rewrite.
- `timestamp` as a `Stamp`: a real RFC 3339 value → `Exact`; an epoch value →
  `ProviderDerived`; a database column → `DatabaseDerived`; only file mtime →
  `FilesystemDerived`; only ordering → `SequenceOnly`; nothing → `Unknown`.
  Always keep the literal in `original`. Never invent a timestamp from a
  neighbour.
- `content` as `Part`s. Unknown block types, encrypted payloads and shapes you
  cannot interpret become `Part::Opaque` with the raw value — **not** a dropped
  record and **not** a fabricated `Text` part.
- Reasoning only from material literally present: plain text → `Public`, a
  provider-authored summary → `Summary`, an encrypted blob → `Opaque` (never
  decoded, never summarized further).
- Large or binary payloads: `ctx.store_artifact(bytes, filename, mime, path)` and
  reference the returned artifact id from the part. Do not inline megabytes into
  an event line.

### 4. Count everything you skip

`ParseReport` carries `conversations`, `events`, `tool_calls`,
`records_examined`, `records_failed`, `records_skipped` and `notes`. The
importer's accounting identity is checked by `convolith validate`, so a record you
handle without counting is a validate failure waiting to happen. Name skipped
record kinds in `notes` (see `docs/SESSION_MIGRATE_MAPPING.md`).

### 5. Register and document

Register the parser in `src/parsers/mod.rs` — most specific first, since registry
order is the detection tie-break. Then:

- add synthetic fixtures under `fixtures/<source>/`,
- add parser tests to `tests/parsers_*.rs` and, for anything end-to-end, to
  `tests/acceptance.rs`,
- append your mapping rows to `docs/SESSION_MIGRATE_MAPPING.md` (one row per
  mapping decision; append only, keep the table flat),
- update the support matrix in `README.md` — and keep the wording honest,
  including "experimental" when that is what it is,
- if you need a new `Part` type, `event_type` or id derivation, update `spec/`
  and the JSON Schemas in the same PR, and say so in `CHANGELOG.md`.

### 6. Fixtures

Use synthetic content. Do **not** commit real transcripts, real paths, real
machine names or anything containing a credential. A fixture should be the
smallest input that exercises the record kind you are mapping. Malformed
variants (truncated line, wrong type, missing timestamp) belong next to the good
one.

## Canonical-format changes

`spec/` is a public contract, not an implementation detail.

- Backwards-compatible: new optional field, new `Part` type, new `event_type`,
  new redaction `kind`, new parser. Readers must ignore unknown values.
- Breaking: changing an id derivation, a `content_fp` rendering, a timestamp
  interpretation, or the meaning of an existing field. Requires a new
  `schema_version`, a `spec/` update, a validated example, and a
  `CHANGELOG.md` entry.
- When you change a serialization in `src/model.rs`, change the matching JSON
  Schema and example in the same commit, and run
  `sh spec/validate-examples.sh`.

## Style

- `cargo fmt` and `cargo clippy -- -D warnings` must be clean. CI fails otherwise.
- Prefer the standard library and the dependencies already in `Cargo.toml`. A new
  dependency needs a reason in the PR description; a "nicer" HTTP/date/CLI
  library is not one.
- No `unwrap()` on anything derived from input. `expect()` only for a genuinely
  impossible state, with a message.
- Errors carry context (`with_context`) and name the path involved.
- Comments explain *why*, not *what*; a non-obvious decision should say what it
  protects against.
- Code identifiers, `kind` strings and error messages are part of the format:
  changing one is a spec change.

## Commits and pull requests

- Small, focused commits with a real message. Say what changed and why.
- Every PR description states: what changed, how you verified it (exact commands
  and file set), and anything you deliberately did not do.
- Do not include real conversation data, credentials or personal paths in a PR,
  an issue, or a test fixture.

## Reporting bugs and security issues

- Functional bugs: open an issue with the command, the smallest synthetic input
  that reproduces it, and the observed versus expected output. Include
  `convolith validate` output when a dataset is involved.
- Security issues: see [SECURITY.md](SECURITY.md) — report privately, do not
  attach live credentials or real transcripts.

## License

By contributing you agree your contributions are licensed under the MIT License
(see [LICENSE](LICENSE)), and you confirm you have the right to submit them.
Code ported from another project must be MIT-compatible and credited in
`docs/PRIOR_ART.md`.