# `convolith all` export — schema `convolith.all/v1`

`convolith all ARCHIVE --output convolith-all.jsonl` (or `--stdout`) flattens an archive
into **one JSONL file**: every canonical event of every provider, machine, conversation
and agent, in a single global chronological order. Exactly one of `--output`/`--stdout`
is required. The archive is opened read-only (the ledger with `SQLITE_OPEN_READ_ONLY`);
`--output` is written to a temp file beside the target and renamed. With `--stdout` only
JSONL goes to stdout; the summary goes to stderr. JSON Schema:
[`all-export.schema.json`](all-export.schema.json).

## Ordering (deterministic)

Sort key, ascending:

1. **Timestamped events first**, by normalized UTC instant (nanoseconds).
2. confidence rank: `exact` < provider-derived < database-derived < filesystem-derived.
3. original source sequence (`seq`, index within the conversation).
4. `event_id`.

**Events with no usable timestamp** (`timestamp: null`, confidence `sequence-only` or
`unknown`) come after every timestamped event, ordered by `(conversation_id, seq,
event_id)` — so each conversation's untimed events keep their source order. A timestamp
is never invented or interpolated; anchoring untimed events next to their timed
neighbours is deliberately not done. A canonical timestamp that fails to parse is treated
as absent (confidence `unknown`), its verbatim text kept in `timestamp_original`.

Output is byte-identical for the same archive: no hash-map iteration reaches the output,
shards are read in file-name order, keys are totally ordered, JSON object keys are sorted.
Importing the same sources in a different order yields the same event sequence (only
provenance bookkeeping such as `observation_id`/`import_run` differs).

**Dedup:** an `event_id` appears once (first shard occurrence wins; other observations are
in `source_refs`).

## Line shape

All keys are always present; missing metadata is `null` (never invented).

| Field | Notes |
|-------|-------|
| `schema` | `"convolith.all/v1"` |
| `event_id`, `conversation_id`, `session_id`, `seq` | canonical ids; `seq` = ordinal within the conversation |
| `parent_event_id` | canonical value, else resolved from `metadata.parent_native_id` within the same conversation; null if unresolvable |
| `timestamp` | normalized UTC RFC 3339 with 9 fractional digits (so lexical order = chronological), or null |
| `timestamp_original` | verbatim source value or null |
| `timestamp_confidence` | `exact` \| `derived` \| `sequence-only` \| `unknown` |
| `timestamp_confidence_detail` | canonical value: `exact`, `provider_derived`, `database_derived`, `filesystem_derived`, `sequence_only`, `unknown` |
| `role` | `user` `assistant` `system` `tool` `other`, plus `developer` (kept, not folded into `system`) |
| `event_type` | canonical: `message` `reasoning` `tool_call` `tool_result` `compaction` `system_note` `attachment` `agent_spawn` `agent_result` `interruption` `error` `opaque` |
| `provider`, `application`, `model`, `agent` | as canonical |
| `subagent` | null, or `{is_subagent, agent_id, parent_session_id, parent_thread_id, depth}` from the fields the canonical event metadata carries (Claude Code sidechain, Codex thread spawn) |
| `machine_id`, `project_id`, `repository_id`, `working_directory`, `branch`, `commit`, `worktree_id` | as canonical |
| `conversation` | boundary metadata from the ledger: `{title, started_at, ended_at, event_count}` (null if the ledger has no row) |
| `content` | blocks, below |
| `metadata` | canonical event metadata, verbatim (includes `native_id`, `tool_call_ids`, …) |
| `source_refs` | provenance, below |

### Content blocks

| canonical part | block |
|---|---|
| text | `{type:"text", text, truncation?}` |
| reasoning | `{type:"thinking", text, visibility}` (only text literally in the source; opaque stays opaque) |
| tool call | `{type:"tool_call", id, name, arguments}` |
| tool result | `{type:"tool_result", call_id, output, is_error}` — `call_id` equals the matching `tool_call.id` |
| image / file ref / artifact | `{type:"attachment", kind:"image"\|"file"\|"artifact", artifact?, path?, mime?, filename?, source_ref?, size?}` |
| data | `{type:"data", value}` |
| opaque | `{type:"opaque", kind, note?, raw?}` |

### `source_refs`

One entry per ledger observation (`provenance.sqlite`), ordered by source, record index:
`observation_id`, `source_id`, `original_path` (the source's original path),
`source_path` (as discovered), `container_chain`, `record_index`, `record_id`, `parser`,
`parser_version`, `source_sha256`, `first_seen`, `import_run`, `identity_tier`,
`machine_id`, `provider`. Without a ledger the event's embedded provenance is used
(`observation_id: null`).

## Resources

Events are streamed from the shards twice. Memory is bounded by a 128 MiB sort chunk
(sorted runs spill to temp files — beside `--output`, or the system temp dir for `--stdout`
— and are k-way merged) plus O(events) short strings: the dedup set and the
native-id→event-id map for parent resolution (~100–200 bytes per event). 1M events
therefore need a few hundred MB, not the content size.

## Limitations

- Conversation-level metadata that the importer does not persist (e.g. Claude sidechain
  `agent_id` at conversation level) is only available if present in event metadata.
- Cross-conversation order among events with identical instants is by `seq`, which is only
  meaningful within one conversation.
- A conversation whose clock goes backwards is ordered by timestamp, not source order;
  use `conversation_id` + `seq` to recover source order.
