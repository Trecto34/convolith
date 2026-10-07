# convolith canonical format v1

Public, versioned specification of the canonical dataset `convolith` produces.

This document is written so that a third party can read a dataset **without
running `convolith`**, without reading Rust, and without this repository: the
canonical data is plain Zstandard-compressed JSON Lines plus SQLite, and every
identifier is a pure function of source coordinates.

Status: **v1 is draft-but-frozen in shape**. `schema_version`/`format_version`
are `1`. Additive changes (new optional fields) stay v1; anything that changes
the meaning of an existing field or an id derivation requires v2 and a new
`schema_version`.

Normative JSON Schemas (JSON Schema 2020-12):

| Schema | Describes |
|--------|-----------|
| [`event.schema.json`](event.schema.json) | one canonical event (authoritative history) |
| [`conversation.schema.json`](conversation.schema.json) | conversation aggregate; `$defs/session` for the session aggregate |
| [`artifact.schema.json`](artifact.schema.json) | one stored artifact |
| [`provenance.schema.json`](provenance.schema.json) | one provenance observation of an event |

Examples that validate against those schemas: [`examples/`](examples/).

---

## 1. Design rules

1. **Nothing is dropped silently.** Every source record is either imported,
   reported as a duplicate, counted as skipped with a reason, or counted as
   failed. The accounting identity
   `records_examined = imported + duplicate + skipped + failed`
   holds per source and per run (checked by `convolith validate`).
2. **Provenance is not optional.** Every canonical event carries at least one
   observation naming the source, the position inside it, and the parser.
3. **Identity is deterministic.** Re-importing the same bytes produces the same
   ids, the same content fingerprints, and no new rows.
4. **Deduplication is conservative.** A false merge loses history; a leftover
   duplicate costs disk. Every rule is biased towards keeping records apart.
5. **Reasoning is never invented.** Only reasoning material literally present in
   the source is emitted; encrypted or vendor-hidden chain-of-thought stays an
   opaque reference.
6. **Sources are read-only.** The tool never modifies, and never executes,
   anything it reads.
7. **Derived data is disposable.** Event shards and the source/observation
   records in `provenance/provenance.sqlite` are authoritative. Search indexes,
   aggregate tables/files and reports can be deleted and rebuilt.

## 2. Dataset layout

```text
canonical-ai-history/
  manifest.json                           dataset metadata + shard index + run log
  README.md                               generated human overview
  schema/*.schema.json                    this specification, copied in
  data/events/part-000001.jsonl.zst       APPEND-ONLY canonical events (authoritative)
  data/aggregates/conversations.jsonl.zst derived summaries (regenerable)
  data/aggregates/sessions.jsonl.zst
  data/aggregates/projects.jsonl.zst
  data/aggregates/machines.jsonl.zst
  artifacts/sha256/aa/<64-hex>            content-addressed blobs
  indexes/search.sqlite                   derived FTS index (rebuildable)
  provenance/provenance.sqlite            durable ledger: sources, observations, conflicts, parse errors
  reports/                                IMPORT_REPORT.md, SOURCE_COVERAGE.md,
                                          DUPLICATES.md, CONFLICTS.md, PARSE_ERRORS.md,
                                          DATA_QUALITY.md, PRIVACY_AUDIT.md,
                                          SOURCE_INVENTORY.md, SOURCE_INVENTORY.json
  checksums.sha256                        `<sha256>  <relative path>` for every produced file
  staging/                                scratch; safe to delete
```

A directory is recognised as a dataset when `manifest.json` or
`provenance/provenance.sqlite` is present. Event shard names are
`part-` + zero-padded decimal + `.jsonl.zst`; the next shard number is one past
the highest present.

## 3. `data/events/*.jsonl.zst` framing

- The file is a **standard Zstandard frame** — `zstd -d`, `zstdcat`, or any zstd
  library reads it. No custom container, no header, no footer.
- The decompressed stream is **UTF-8 JSON Lines**: one JSON object per line,
  each terminated by a single `\n` LF. Blank lines are ignored on read; writers
  never emit them.
- One line = one canonical event ([`event.schema.json`](event.schema.json)).
  Records are written one at a time and never pretty-printed.
- Shards are **append-only across runs**: a new import adds new files
  (`part-000002.jsonl.zst`, ...) and never rewrites an existing shard. An
  interrupted run leaves no partial shard (writes land in a `.tmp<pid>` file and
  are renamed on success).
- A record larger than the configured record limit is refused (bounded
  buffering). Large payloads are moved to the artifact store, so event lines
  stay small and streamable.
- Reading is tolerant: an unparseable line is counted and reported with its line
  number, never silently skipped.

Decoding by hand:

```sh
zstd -dc dataset/data/events/part-000001.jsonl.zst | head -1 | jq .
sqlite3 dataset/provenance/provenance.sqlite \
  "select * from observation where event_id='ev_0123456789abcdef01234567'"
```

## 4. Identifier derivation

All canonical ids are `prefix` + the **first 24 lowercase hex characters** of a
BLAKE3 digest (96 bits) over a tuple of length-prefixed strings, so `("ab","c")`
and `("a","bc")` cannot collide. All digests of *bytes* in the dataset are
SHA-256 instead (`artifacts/**`, `checksums.sha256`, `source_sha256`), because
that is what external tooling verifies.

`H(x…)` below means: `hex(BLAKE3( Σ le64(len(x_i)) ‖ x_i ‖ 0x1f ))`, then take
the first 24 hex chars.

| Id | Prefix | Inputs | Notes |
|----|--------|--------|-------|
| Event (native tier) | `ev_` | `H(provider, application, "native", native_id)` | Provider id that is globally unique on its own (a UUID, or a long mixed-alphabet token). Collapses across snapshots even when a session header is missing. |
| Event (coordinate tier, provider id) | `ev_` | `H(provider, application, "coord", conversation_id, native_id)` | Provider id only unique *within* a conversation (`"msg_1"`). |
| Event (coordinate tier, no id) | `ev_` | `H(provider, application, "pos", conversation_id, seq, role, event_type)` | Stable position inside the conversation. |
| Event (fingerprint tier) | `ev_` | `H(provider, application, "fp", conversation_id, seq, role, event_type, content_fp)` | Nothing stable. Collapses only records that agree on position **and** content. |
| Conflict variant | `ev_` | `H("variant", event_id, content_fp)` | A second record claiming an existing identity with different content. Both are kept (see §7). |
| Conversation | `cv_` | `H(provider, application, "native", id)` with `id` = provider conversation id, else provider session id — both only if globally unique; otherwise `H(provider, application, "coord", title, working_directory, source path)` | The native spelling is used when a globally-unique provider id exists. |
| Session | `se_` | `H(provider, application, native_session_id)` | Only when the source has a session distinct from the conversation. |
| Project | `prj_` | alias: `H("alias", name)`; else repo: `H("repo", normalized_remote)`; else `H("path", normalized_directory)` | Path alone never merges two projects into one across machines unless the path spelling is identical. |
| Repository | `repo_` | `H("alias", alias)` or `H("remote", normalized_remote)` | Remote is normalised (scheme/user/case-insensitive, `.git` dropped). |
| Source | `src_` | `H(display_path, container_chain…)` | Identifies the physical file plus its archive ancestry. |
| Artifact | `art_` | `H("sha256", sha256)` | Content-addressed: identical bytes are one artifact. |
| Import run | `run_` | `HHH…` = first 12 hex chars of `H(started_at, pid)` | Not content-addressed; a run is an event in time. |

**Content fingerprint** (`content_fp`) is a BLAKE3 digest of the event's stored
content parts, each rendered canonically (`t:<text>`, `r:<vis>:<text>`,
`c:<id>:<name>:<json-args>`, `o:<id>:<is_error>:<json-output>`,
`i:…`/`f:…` image/file refs, `a:<artifact>:<size>:<mime>:<filename>`,
`d:<json>`, `x:<kind>:<note>:<json-raw>`), with a truncated text part rendered as
`t:<full_sha256>:<inline_bytes>`. It is computed **after** secret policy and
truncation, so two runs agree even when the policy differs from the source
bytes. Content is never hashed alone: at every tier it is combined with
provider, application, conversation, sequence, role and event type.

Globally-unique-id test (used to choose the native tier): trimmed length ≥ 16
**and** either a canonical UUID (`8-4-4-4-12` hex) or a part of ≥ 32 hex digits,
**or** length ≥ 20 with at least one letter, one digit, and ≥ 12 distinct
ASCII bytes. A bare `"1"` or `"msg_3"` is deliberately not global.

## 5. Timestamps and confidence

- `timestamp` is RFC 3339 in **UTC with a `Z` suffix**, second or nanosecond
  precision: `YYYY-MM-DDTHH:MM:SS(.fffffffff)Z`. Local times without an offset
  are never guessed.
- `timestamp_original` keeps the literal text found in the source, even when it
  cannot be parsed.
- `timestamp_confidence` says how much the value is trusted:

| Value | Meaning |
|-------|---------|
| `exact` | Absolute instant with an explicit UTC offset, from the source itself. |
| `provider_derived` | Absolute instant derived from a provider-encoded epoch value. |
| `database_derived` | Absolute instant taken from a database column. |
| `filesystem_derived` | Absolute instant taken from the filesystem, not from the record. |
| `sequence_only` | Only ordering within the source is known; `timestamp` is absent. |
| `unknown` | Nothing usable was found; `timestamp` is absent. |

Timestamps are never taken from file mtime or from neighbouring records unless
the confidence field says so.

## 6. Provenance

Two representations of the same facts:

1. **In-band**: the event schema supports `provenance[]` — an array of
   [`provenance.schema.json`](provenance.schema.json) objects, smallest-first
   (`source_id`, `source_path`, `container_chain`, `record_index`, `record_id`,
   `parser`, `parser_version`, `source_sha256`, `first_seen`, `import_run`,
   `identity_tier`). Current imports store observations in the ledger; the
   event shard's inline list may be empty and is not a substitute for it.
2. **Durable ledger**: `provenance/provenance.sqlite` is part of the canonical
   archive. Its `source`, `observation`, `conflict` and `parse_error` records
   preserve source identity, original path, machine, provider, record refs and
   session associations. These records cannot be reconstructed from event
   shards alone. The database is included in `checksums.sha256` and checked by
   `convolith validate`. Back it up and copy it with the event shards and
   manifest; copying only events and the manifest loses provenance and will
   not validate. Tables:

| Table | Contents |
|-------|----------|
| `meta` | key/value: dataset and ledger schema version. |
| `source` | one row per discovered source: path, original path, container chain, size, mtime, sha256, machine, platform, provider/application guess, format, parser, parser version, detection confidence, status, import run, first/last seen, and record accounting (`records_examined/imported/duplicate/skipped/failed`). |
| `source_state` | resume bookkeeping: `(path, size, mtime, parser, parser_version)` fingerprint + `complete`. An unchanged, complete source is skipped on `--resume`. |
| `event_index` | one row per canonical event: conversation, session, seq, role, event type, provider, application, project, repository, machine, timestamp, `content_fp`, `identity_tier`, shard, `first_seen`, `observation_count`, `variant_of`. |
| `observation` | one row per (event, source record). Unique on `(event_id, source_id, ifnull(record_index,-1), parser)`, so re-importing one record is idempotent while two distinct sources can observe the same event. |
| `conflict` | `(event_id, variant_event_id, kind, detail, content_fp, source_id, source_path, observed_at)`. |
| `parse_error` | `(source_id, source_path, locator, message, import_run, at)`. |
| `conversation`, `conversation_source`, `session`, `project`, `project_path`, `project_branch`, `machine`, `machine_evidence`, `artifact_ref`, `artifact_event` | aggregate tables; regenerable from `event_index` + `observation` via `rebuild-index`. |

`observation_count > 1` means several source records corroborated one event;
`convolith provenance <event_id>` prints all of them, and `DUPLICATES.md` lists
the corroborated ones.

## 7. Deduplication and conflict semantics

Local `collect` machine ids are `windows/<24 hex>` or `linux/<24 hex>`:
the first 96 bits of the domain-separated BLAKE3 tuple
`("convolith-local-machine-v1", platform, trim(lowercase(OS installation id)))`.
Windows reads the 64-bit registry `HKLM\SOFTWARE\Microsoft\Cryptography` value
`MachineGuid`; Linux reads `/etc/machine-id` or `/var/lib/dbus/machine-id`.
The raw OS identifier and its filesystem location are not machine ids. A missing
identifier fails only the local machine. WSL/SSH ids remain `wsl/<distro>` and
`ssh/<host>` for compatibility; event/source identity does not include machine id.

Legacy archives with generic `linux`/`windows` need no schema migration. Preserve
their shards, canonical ids and observations. Newly encountered sources use the
stable id while historical sources/events retain their original machine field. Do not
relabel a generic machine: an archive may contain several physical computers.
Legacy collect-state root keys are not aliased to a new machine; first collection
revisits the roots and ordinary source resume/dedup applies, then new root-state
keys make future unchanged runs skip normally. Old keys remain readable.

For each emitted event, the importer looks up `event_id` in the ledger:

- **New** — unseen identity: the event is written to the current shard, an
  observation is added, `first_seen` set, accounting `imported += 1`.
- **Duplicate** — same identity *and* same `content_fp`: nothing is written; one
  more observation row is added (idempotent by the unique index);
  accounting `duplicate += 1`.
- **Conflict** — same identity, different `content_fp`: no winner is picked.

  The variant is preserved as its own event with
  `event_id = H("variant", event_id, content_fp)` and metadata
  `variant_of: <original event_id>` plus
  `conflict_existing_content_fp: <existing fingerprint>`. The ledger records
  `kind = "same_identity_different_content"` with the detail
  `"the same canonical identity was observed with different content; both
  variants are preserved"`, and the case is listed in `reports/CONFLICTS.md`
  and in `DATA_QUALITY.md`. Conflict recording is **idempotent**: re-importing
  the same conflicting pair adds no `conflict` row and no second variant event.

  The original event's `event_index.variant_of` is `NULL`; the variant's is the
  original id, so the whole conflict set is reachable by one query:

  ```sql
  select event_id, variant_of from event_index where variant_of is not null;
  ```

Practical consequences:

- Two snapshots of the same store collapse (same ids, same fingerprints) and
  produce observations, not duplicates-by-disk.
- A resumed/forked session file that repeats records with the same UUID
  collapses onto the same event.
- A redaction-policy change alters `content_fp`, so it is reported as a conflict
  rather than overwriting silently.
- A source that yields no stable ids lands in the fingerprint tier, which is the
  tier that can never over-merge different messages.

## 8. Redaction

Canonical, searchable content is subject to the secret policy recorded in
`manifest.json.redaction_policy`:

- `redact` (default) — matched spans in *stored content* are replaced by a
  stable marker `[REDACTED:<kind>]`. The pattern is applied to every string in
  content, including tool arguments and tool outputs — not only prose.
- `preserve` — stored content keeps the original text; the audit still counts
  what was found, so the operator can see the exposure without it leaking into
  logs, reports or the manifest.

Either way:

- The **originals on disk are never modified.**
- Each event carries `redactions[]` with `kind` (`openai_key`, `private_key`,
  `jwt`, `password_assignment`, …, or `multiple(a+b)` when several matched) and
  the JSON `field` it applied to, e.g. `content[0].text`.
- `reports/PRIVACY_AUDIT.md` contains counts only — no secret value ever reaches
  a report, a log, or `manifest.json`.
- A redacted event is counted in `counts.redacted_events`.
- Truncation is not redaction: text longer than the inline limit is kept whole
  in the artifact store (hashed *after* redaction) and referenced by
  `content[i].truncation = { full_sha256, full_bytes, inline_bytes, artifact }`.

## 9. `manifest.json`

```jsonc
{
  "format": "convolith-canonical",
  "format_version": 1,
  "schema_version": 1,
  "tool": "convolith",
  "tool_version": "0.1.0",
  "created_at": "2025-01-01T00:00:00Z",
  "updated_at": "2025-01-02T00:00:00Z",
  "redaction_policy": "redact",
  "counts": {
    "sources": 0, "conversations": 0, "sessions": 0, "events": 0,
    "tool_calls": 0, "artifacts": 0, "projects": 0, "machines": 0,
    "conflicts": 0, "redacted_events": 0,
    "date_range": ["2024-01-01T00:00:00Z", "2024-12-31T23:59:59Z"]
  },
  "event_shards": [
    { "path": "data/events/part-000001.jsonl.zst", "records": 0, "bytes": 0,
      "sha256": "…", "import_run": "run_0123456789ab" }
  ],
  "derived_files": [ { "path": "…", "records": 0, "bytes": 0, "sha256": "…", "import_run": "" } ],
  "import_runs": [
    { "import_run": "run_0123456789ab", "started_at": "…", "finished_at": "…",
      "tool_version": "0.1.0", "schema_version": 1, "secret_policy": "redact",
      "args": [], "events_new": 0, "events_duplicate": 0, "sources_examined": 0,
      "sources_skipped": 0, "sources_failed": 0, "parse_errors": 0,
      "state": "complete", "notes": [] }
  ],
  "notes": []
}
```

`counts` describes the whole dataset, not one run. `event_shards` is sorted by
path; `derived_files` covers regenerable outputs. `date_range` is `null` when no
event has a usable timestamp. Counts are recomputed from the ledger on every
run, so a hand-edited `manifest.json` is corrected rather than trusted.

## 10. Checksums and verification

`checksums.sha256` is sorted, uses `<sha256>  <relative/posix/path>` lines
(two spaces, exactly like `sha256sum`), and excludes itself and files that may
change after it is written (`indexes/`, `staging/`). Writing it twice over
unchanged content produces byte-identical output.

```sh
cd dataset && sha256sum -c checksums.sha256
```

`convolith validate <dir>` re-derives every check from the data and prints
`PASS`/`FAIL` per check: manifest readability, required paths and shards,
checksums (including the durable provenance ledger), event ids and shard/ledger agreement, provenance for every event,
record accounting, referential integrity. It exits `1` on any `FAIL`.

## 11. Compatibility and change policy

- `schema_version` (canonical record shape) and `format_version` (dataset
  layout) are independent and both `1` here.
- Adding an **optional** field, a new `Part` type, a new `event_type`, a new
  redaction `kind`, or a new parser is a backwards-compatible v1 change:
  readers must ignore unknown `type`/`event_type` values rather than fail, and
  should treat unknown `metadata` keys as opaque.
- Changing an id derivation, a `content_fp` rendering, a timestamp
  interpretation, or the meaning of an existing field requires a new
  `schema_version` and a migration note in `CHANGELOG.md`.
- `kind` in a provenance observation (`opaque`, `encrypted_reasoning`,
  `attachment:<type>`, …) is provider-defined and open-ended.

## 12. Official web-chat exports (parser notes)

Informative; the canonical record shape is unchanged. Parsers `chatgpt_export`,
`claude_web_export`, `gemini_takeout` and `perplexity_export` read the files of the
providers' own data downloads (zip members or extracted files). Verification status:
`gemini_takeout` for the Takeout *Conversation History* shape is verified against a real
export; every other shape is **unverified-against-real-export** (built from the documented
layout, tested with synthetic fixtures only).

* **Provenance**: `source_path` is `<zip>!/<member>` (or the file path), `container_chain`
  holds the zip name(s); `record_id` is the provider message id. `machine_id` is the
  importing machine unless the config names one.
* **Identity** (tier `native`): conversation key = provider conversation id (ChatGPT
  `conversation_id`, Claude `uuid`, Perplexity thread id; Gemini Takeout has none, so
  `gemini-takeout:<file id>:<creation_time>` is used). Event key = provider message id (ChatGPT
  `message.id`, Claude message `uuid`); Gemini and Perplexity derive `<conversation
  key>:<role>:<turn_index>` / `<entry id>:query|answer`. Ids that are not globally unique
  (not a UUID or long token) are demoted to the coordinates/fingerprint tiers by the
  ordinary rules. The key includes provider and application, so a web export merges
  with another source only when both agree on provider, application and id; text alone
  never merges (§7).
* **Timestamps**: `timestamp_original` keeps the provider value verbatim (epoch float or
  ISO string), `timestamp` is its UTC instant; a missing time stays missing.
* **Branches**: ChatGPT `mapping` nodes are all emitted; `parent_native_id` points to the
  nearest ancestor carrying a message. Claude exports are flat; a parent link exists only
  when the export provides `parent_message_uuid`.
* **Unknown versions**: files that look like an export but match no known shape (or a
  Perplexity file declaring another `version`) are inventoried as `unsupported` with a
  reason; individual malformed conversations are counted as failed records.
