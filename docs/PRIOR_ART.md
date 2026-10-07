# Prior art

convolith is not a new idea. Plenty of people have hit "my AI history is
fragmented" and written something. This file records what already exists, what
was taken from it, and what was deliberately not taken — so that a reader can
judge whether convolith is worth using and so that no debt is hidden.

Rules for this file: state the actual relationship (code copied / semantics
ported / idea only / unrelated), name the license when code or semantics were
reused, and never claim a comparison that was not checked. Licenses below are as
published at the time of writing; verify before reusing anything.

## Reused — semantics ported

### [xhluca/session-migrate](https://github.com/xhluca/session-migrate) — MIT

**What it is.** A converter between agent session formats (Claude Code, Codex.cli,
Gemini CLI, and others), implemented as a Python package with a per-format
"dialect" model.

**What convolith took.** The *semantics* of each record kind — which record is a
message, which is a tool call, which is bookkeeping that should be skipped, how a
subagent sidechain relates to its parent session, how a compacted/summary record
should be represented. These are the expensive part: they are format knowledge
recovered from undocumented private formats, not code.

**What convolith did not take.** No source code was copied or translated. The work
is recorded per decision in
[`SESSION_MIGRATE_MAPPING.md`](SESSION_MIGRATE_MAPPING.md), which keeps a row for
every mapping choice so the debt is visible and attributable. MIT is compatible
with convolith's MIT license, but attribution is treated as an obligation
regardless.

### IronBridge — architecture level only

**What it is.** A project in the same problem space (canonicalizing and
preserving AI conversation history, with provenance as a first-class concern).

**What convolith took.** Nothing concrete. Reading it confirmed that the problem is
worth solving and that provenance-first framing is the right shape; the canonical
model, ids, ledger schema, dedup tiers and dataset layout here were designed
independently.

**What convolith must not take.** IronBridge is AGPL-licensed. No code, schema,
text or data file from it may be copied into this repository, and this project's
architecture must stay independently derived. If you are reading IronBridge while
writing a parser, make the same argument work without its source in front of you.
The ADRs were not ported.

## Surveyed, idea level only

The categories below were looked at to make sure convolith was not duplicating an
existing answer. None of them contributed code, schema or format knowledge.

| Project / category | What it does | Relationship |
|--------------------|--------------|--------------|
| ChatGPT/Claude export converters (e.g. `chatgpt-exporter`-style tools) | Turn one platform's export into Markdown, HTML or JSON for reading | Different output contract: human-readable rendering, no canonical event model, no provenance, no dedup. Useful prior art for *what a record is*; none of it was reused. |
| Session log viewers (`claude-code-log`, and similar) | Render one agent's local JSONL into a browsable page | Single-format, presentation-first. Confirms which record kinds exist; contributes no canonical model. |
| Usage/cost trackers | Aggregate token usage and spend from local session stores | Same input files, different question (billing, not history). Adjacent, not competing; no overlap in output. |
| Note-taking "import your AI chats" features (Obsidian/Notion plugins and similar) | Pull conversations into a notes vault | Destination is a human document, not an auditable dataset. Lossy by design. |
| Backup/rsync-style tooling | Copy the session directories somewhere else | Preserves bytes, not meaning: no identity, no dedup across snapshots, no provenance, no conflict handling. convolith is what you run *after* (or instead of) treating a backup as an archive. |
| SQLite-based chat history apps | Live UI over one local database | Reads a single store. convolith's hard part — the same conversation observed across several stores, backups and forks — is exactly what they do not handle. |
| General-purpose ETL / `jq` / log pipelines | Transform JSONL | Not competitors: convolith's value is the format knowledge and the identity/provenance rules, which a generic pipeline cannot supply. |

## Deliberately not solved

- **Fetching live data.** No provider API client, no scraping, no server-side
  export. convolith reads what you already have on disk.
- **Attachments over the network.** A referenced image or file whose bytes are
  not present locally stays a reference (`source_ref`), never a download.
- **Rendering.** Reports and search results are for operators; a rich transcript
  viewer is out of scope.
- **Sanitization.** Redaction is damage limitation, not a guarantee — see
  [SECURITY.md](../SECURITY.md).
- **Modifying the sources.** Ever. Not deduplicating them, not renaming them, not
  rewriting their timestamps.

## Correction policy

If this file misstates what a project does, what it is licensed under, or what
was taken from it, that is a bug: open an issue or a PR and it will be fixed.
Attribution errors are treated with the same seriousness as data-loss bugs.