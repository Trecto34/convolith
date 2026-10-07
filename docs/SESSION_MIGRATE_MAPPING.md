# session-migrate → convolith mapping

Format knowledge for several parsers comes from
[xhluca/session-migrate](https://github.com/xhluca/session-migrate) (MIT). convolith
ports the *semantics* (what each record kind means), not the code. Each parser
owner appends their own rows below; keep one row per mapping decision.

| Parser | Source concept | convolith mapping | Note |
|--------|----------------|-----------------|------|
| claude_code | One JSONL file per session under `~/.claude/projects/<dir>/`; `sessionId` | One conversation, `native_id = sessionId`; `cwd`, `gitBranch` from the first record that names them | Header is learned from the first record carrying `sessionId` + `cwd`; earlier records are buffered (≤256), never reordered |
| claude_code | Record `uuid` / `parentUuid` | `native_id` / `parent_native_id` (event id is derived from the uuid) | The `parentUuid` tree is kept verbatim: retries and forks are siblings; copies of a record in a resumed/forked file collapse onto the same event and add a provenance observation |
| claude_code | `type: user` / `assistant` | `message` events, role from `message.role`, `model` from `message.model` | `message.id`, `stop_reason`, `usage`, `requestId` kept in event metadata; assistant API messages split over several records stay several events |
| claude_code | `tool_use` / `server_tool_use` / `mcp_tool_use` block | `ToolCall` part (`id`, `name`, `input` → `arguments`); event type `tool_call` when the record has no text | |
| claude_code | `tool_result` block (in a `user` record) | `ToolResult` part (`tool_use_id`, `content` verbatim, `is_error`); a record of only tool results is role `tool`, event type `tool_result` | Source role (`user`) kept in metadata `source_role` |
| claude_code | `thinking` block | `Reasoning` (public) only when the text is non-empty; a signature-only block becomes an opaque `thinking` part without text; `redacted_thinking` stays opaque raw | No reasoning is ever synthesised |
| claude_code | `isSidechain`, `agentId`, `<session>/subagents/agent-*.jsonl` | Sidechain file = separate conversation `<sessionId>/agent-<agentId>` (never merged into the parent); events carry `agent`, metadata `is_sidechain`, `agent_id`; conversation metadata `parent_session_id` | Spawning `Task` call stays an ordinary `tool_call` in the parent |
| claude_code | `summary`, `ai-title`, `custom-title` | `system_note` events with the text | `leafUuid` kept as `leaf_uuid` |
| claude_code | `system` (`compact_boundary` …) | `system_note`, or `compaction` for `compact_boundary`; `compactMetadata` / `logicalParentUuid` in metadata | |
| claude_code | `attachment` | `attachment` event, role `other`, opaque part `attachment:<type>` with the raw object | |
| claude_code | `isCompactSummary` user message | event type `compaction`, role user | |
| claude_code | `file-history-*`, `progress`, `queue-operation`, `last-prompt`, `permission-mode`, `mode`, `bridge-session`, `cost-state`, `atis-latch` | Not imported; counted as skipped records and named in the report notes | `toolUseResult` side-channel on user records is likewise not imported |
| claude_code | Unknown record `type` | `opaque` event with the raw record | Nothing is dropped silently |
| claude_code | `timestamp` | RFC 3339 → `exact`; absent → `unknown`; unparsable → `unknown` with the literal kept in `timestamp_original` | Never taken from file mtime or neighbours |
| claude_code | Images / documents in content | Opaque part with the raw block (base64 inline) | Upgrade path: decode into artifacts |
| codex | `~/.codex/sessions/**/rollout-*.jsonl`; `session_meta.payload.id` | One conversation, `native_id = payload.id`; `cwd`, `git.branch`, `git.repository_url` from the header; filename uuid used only when the header is missing | Legacy untyped header (`{id, timestamp, instructions}`) and bare items are also read |
| codex | `session_meta` | `system_note` event holding `base_instructions.text` (or legacy `instructions`); `originator`, `cli_version`, `source`, `forked_from_id`, `git` in metadata | `source` object with a `subagent` key → `is_subagent` |
| codex | `response_item` `message` | `message`, role from `payload.role` (`user`/`assistant`/`developer`/`system`); `payload.id` is the native id when present | No id in the source → none invented (fingerprint tier) |
| codex | `response_item` `reasoning` | `Reasoning` summary parts (`summary_text`) and public parts (`content`); `encrypted_content` kept as opaque `encrypted_reasoning`, never decoded | |
| codex | `function_call`, `custom_tool_call`, `local_shell_call`, `web_search_call` | `ToolCall` part; `arguments` JSON string parsed (kept as text when not JSON); native id `<call_id>:call` | |
| codex | `function_call_output`, `custom_tool_call_output` | `ToolResult` part, role `tool`; `is_error` only when the output object says `success: false`; native id `<call_id>:output` | |
| codex | `turn_context` | Not an event; its `model` / `cwd` are applied to the events that follow (`model` on assistant-side events only) | Counted as skipped |
| codex | `event_msg` `user_message` / `agent_message` / `agent_reasoning*`, `item_completed` of `UserMessage` / `AgentMessage` / `Reasoning` | Not imported: echoes of `response_item`; counted as skipped | |
| codex | `event_msg` `item_completed` (other kinds: `CommandExecution`, `FileChange`, `SubAgentActivity`, …) | `opaque` event with the raw item | Carries exit codes / file changes that the response items lack |
| codex | `event_msg` `turn_aborted` / `error` | `interruption` / `error` event with the reason / message | |
| codex | `compacted` | `compaction` event: `message` text plus an opaque part with the rest (replacement history, window ids) | |
| codex | `agent_message` response item | `message`, role `other`, `author` / `recipient` in metadata | |
| codex | `token_count`, `token_usage_record`, `world_state`, other `event_msg` kinds | Not imported; counted as skipped and named in the report notes | |
| codex | Top-level `timestamp`, `ordinal` | `timestamp` as for Claude Code; `ordinal` kept in metadata | Legacy bare items have no timestamp → `unknown` |
| chatgpt | ChatGPT `mapping` node keys | event native IDs and `chatgpt_node_id` | Every message-bearing node is emitted, retaining forks |
| chatgpt | node `parent`, `children` | draft parent ID and metadata | Preserved as source coordinates |
| chatgpt | conversation `current_node` | conversation/event metadata | Retained to identify selected branch |
| chatgpt | message `author.role` | event role | Mapped through canonical role vocabulary |
| chatgpt | message `create_time` | event timestamp | Epoch seconds, fractional seconds retained; absent values remain unknown |
| chatgpt | message `attachments` | `FileRef` plus opaque reference | No remote bytes are fetched |
| pi / oh_my_pi | `~/.pi/agent/sessions/**/<timestamp>_<uuid>.jsonl` (Oh My Pi: `~/.omp/agent/…`); `session` header `{version, id, cwd, timestamp, parentSession}` | One conversation, `native_id = id`; `cwd` → working directory; `parentSession` in conversation metadata. The header line also becomes a `system_note` with the raw header | Oh My Pi is the same parser registered as `oh_my_pi` (application `oh-my-pi`), told apart by a `.omp` path component or its leading `title` record; the two never both claim a file |
| pi / oh_my_pi | Entry `id` / `parentId` | `native_id` / `parent_native_id` verbatim; every branch of the tree is imported (session-migrate projects only the active path) | Entry ids are only unique per file, so identity is scoped by conversation (coordinates tier) |
| pi / oh_my_pi | `message` entry, role `user` / `assistant` | `message` event; assistant `model`, `provider`, `api`, `usage`, `stopReason`, `errorMessage`, `responseId` kept (model on the event, rest in metadata) | Event type `tool_call` / `reasoning` when the message has no text, as for Claude Code |
| pi / oh_my_pi | `toolCall` block / `toolResult` message | `ToolCall` part (`id`, `name`, `arguments`); `ToolResult` part (`toolCallId`, `content` verbatim, `isError`), role `tool`, `toolName` / `details` in metadata | |
| pi / oh_my_pi | `thinking` block | `Reasoning` (public) only when text is present; signature-only → opaque `thinking` without text | Nothing synthesised |
| pi / oh_my_pi | `bashExecution` message | `message`, role user: an id-less `bash` `ToolCall` + `ToolResult` pair; `exitCode`, `cancelled`, `truncated`, `excludeFromContext` in metadata | User-run `!cmd`; `is_error` only when `exitCode` ≠ 0 |
| pi / oh_my_pi | `compaction`, `compactionSummary`, `branch_summary`, `branchSummary` | `compaction` event with the summary text (`firstKeptEntryId`, `tokensBefore`, `details` in metadata) / `system_note` | |
| pi / oh_my_pi | `custom_message`, `custom` message role | `message`, role `other`, `customType` / `display` in metadata | `custom` entries (extension state) are `opaque` |
| pi / oh_my_pi | `model_change`, `thinking_level_change`, `label`, `session_info`, Oh My Pi `mode_change` / `title_change` / `reset_boundary` … | `system_note` (text for `session_info.name`, opaque raw otherwise); unknown entry types → `opaque` | Nothing dropped |
| pi / oh_my_pi | Oh My Pi 256-byte `title` slot (first line) | Conversation title; counted as one skipped record | |
| pi / oh_my_pi | Entry `timestamp` (ISO) / `message.timestamp` (epoch ms) | Entry timestamp, else the message's own; absent → `unknown` | Never taken from the file name or mtime |
| opencode | `storage/session/<project>/<ses_id>.json`, `session` table row, `opencode export` bundle | One conversation per session, `native_id = id`; `directory`, `title`, `time.created/updated`; `version`, `projectID`, `parentID` (→ `parent_session_id`), `slug` in metadata | session-migrate reads only export bundles; convolith also reads the file store and `opencode.db` through `src/sqlite.rs` (copy + WAL replay) |
| opencode | `storage/message/<ses>/*.json`, `storage/part/<msg>/*.json` | Read by the session source in id order. A message or part file met on its own is claimed (so it is not "unsupported") but yields nothing | A session file in a staged archive without its sibling directories imports no events and says so in the report notes |
| opencode | `message` `role`, `parentID`, `modelID`/`model`, `agent`, `mode`, `cost`, `tokens`, `finish`, `error`, `summary`, `system` | `message` event (id = `native_id`, `parentID` = `parent_native_id`); `providerID` and the rest in metadata | |
| opencode | `text`, `reasoning` parts | `Text`; `Reasoning` (public) only when text is present | |
| opencode | `tool` part (`callID`, `tool`, `state`) | `ToolCall` part in the message event; a `completed` / `error` state also yields a `tool_result` event (role `tool`, native id `<part id>:result`, `output` / `error`, `is_error`, `title`, `metadata`); `pending` / `running` → listed in `unfinished_tool_calls`, no result invented | |
| opencode | `file` part | `FileRef` for a URL / path; inline `data:` payloads stay an opaque raw part | |
| opencode | `compaction` part, assistant `summary: true` | Event type `compaction` | |
| opencode | `step-start`, `step-finish`, `snapshot` parts | Not imported; counted as skipped (cost / tokens already live on the message) | |
| opencode | `patch`, `agent`, `subtask`, `retry`, unknown part kinds | Opaque part with the raw object | |
| opencode | `todo` table, binary cells, session `summary` / `share` / `revert` / `permission` | Not imported | |
| hermes | `~/.hermes/state.db` `sessions` / `messages` tables (via `src/sqlite.rs`) | One conversation per session row, one event per message row (`id` = `native_id`); `title`, `model`, `cwd`, `started_at` / `ended_at` (epoch seconds); `source`, `parent_session_id`, `end_reason`, counters in metadata | Session id is namespaced `hermes-session-<id>` to be a deterministic, globally unique native id |
| hermes | `sessions.system_prompt` | First event of the conversation: `system_note` with the text | |
| hermes | `~/.hermes/sessions/session_<id>.json`, `<id>.jsonl` | Same message mapping; the JSON log's `session_id`, `model`, `platform`, `session_start`, `last_updated` fill the header; a leading `session_meta` line fills model / platform and is counted as skipped | Naive local ISO text (no offset) is not UTC: kept as `timestamp_original` / `*_original`, instant unknown |
| hermes | `assistant.tool_calls` (OpenAI shape) | `ToolCall` parts; `function.arguments` JSON text parsed (kept as text when not JSON) | |
| hermes | `tool` row (`tool_call_id`, `tool_name`) | `ToolResult` part, role `tool`; `is_error` only when the `{output, exit_code, error}` envelope reports an error or non-zero exit | `content` kept verbatim |
| hermes | `reasoning`, `reasoning_content`, `reasoning_details` | `Reasoning` (public) when text is stored; `reasoning_details` as an opaque part | session-migrate drops these for portability; an archive keeps them |
| hermes | `_compressed_summary = 1` | Event type `compaction` | |
| hermes | `active = 0` rows (compacted / rewound) | Imported with metadata `inactive` (and `compacted`) | session-migrate keeps only an opaque marker |
| hermes | Image `image_url` blocks | `Image` with `source_ref` for http(s); inline `data:` URLs stay opaque | |
