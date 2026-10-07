# `convolith all --harness` export — schema `convolith.harness/v1`

`convolith all ARCHIVE --harness --output convolith-harness.jsonl.zst` exports a compact, normalized interchange format intended for downstream memory, search, and agent harness systems.

By default, `--harness` compresses with zstd when writing to a file. Uncompressed output can be explicitly requested with `--no-compress` (or `--uncompressed`).

## Schemas

- **Event record**: `convolith.harness/v1`
- **Conversation metadata record**: `convolith.harness.conversation/v1`

## Record shapes

### Event record (`convolith.harness/v1`)

```json
{
  "schema": "convolith.harness/v1",
  "event_id": "ev_7ae9649725fd9f10e00c146d",
  "conversation_id": "cv_cd7ca158ef892e450b361843",
  "session_id": null,
  "parent_event_id": null,
  "seq": 0,
  "timestamp": "2023-05-13T15:38:58.895292000Z",
  "timestamp_confidence": "derived",
  "role": "user",
  "event_type": "message",
  "provider": "openai",
  "application": "chatgpt",
  "model": null,
  "content": [
    {
      "type": "text",
      "text": "Hello..."
    }
  ],
  "machine_id": "windows/3af662bbf7efcea9e5097974",
  "project_id": null,
  "repository_id": null,
  "worktree_id": null,
  "hidden": false,
  "source_refs": [
    {
      "source_id": "src_cf0b575eb11831d5ae3f998d",
      "record_id": "aaa2f460-d2bf-4f59-ac60-4b9503181d9f"
    }
  ]
}
```

### Conversation record (`convolith.harness.conversation/v1`)

Conversation-level metadata is emitted once as a dedicated record at the start of the export, avoiding repetition across events:

```json
{
  "schema": "convolith.harness.conversation/v1",
  "conversation_id": "cv_cd7ca158ef892e450b361843",
  "title": "Debate de IA.",
  "started_at": "2023-05-13T15:38:58Z",
  "ended_at": "2023-05-13T15:51:39Z"
}
```

## Key principles

1. **Lightweight provenance**: `source_refs` contains only `{source_id, record_id}` to trace back to the authoritative canonical Convolith archive. Full forensic provenance (hashes, container chains, parser versions) remains in the canonical archive.
2. **Explicit branching**: Alternate responses sharing the same parent retain their independent `parent_event_id` and are not flattened into sequential turns.
3. **Missing/zero timestamps**: Missing, zero, or unparseable timestamps are rendered as `timestamp: null, timestamp_confidence: "unknown"` and sorted into group 1 (trailing timed events). They are never converted into Unix epoch 1970.
4. **Hidden/control events**: Internal records (system reminders, compaction boundaries, visually hidden turns) have `hidden: true` and `event_type: "control"`, allowing downstream memory harnesses to filter them from prompt context while keeping history.
5. **Deterministic ordering**:
   `trustworthy timestamp → timestamp confidence/precision → conversation/session ordering → source sequence → deterministic event_id fallback`.
6. **Streaming with bounded memory**: Uses the same bounded chunk spilling (128 MiB) and k-way merging mechanism as canonical export, compressed with zstd on the fly.
