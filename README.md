# convolith

Loss-preserving, provenance-first canonicalizer for fragmented AI conversation
history. Point it at whatever is on disk — local agent session stores, exported
chats, backups, archives of either — and it produces one canonical dataset where
every event says where it came from, duplicates collapse without being deleted,
and conflicts are preserved instead of resolved.

Written in Rust. No network access. Nothing is ever executed.

```text
canonical-ai-history/
  manifest.json                  dataset metadata, shard index, run log
  data/events/part-*.jsonl.zst   APPEND-ONLY canonical events (authoritative)
  data/aggregates/*.jsonl.zst    regenerable summaries
  artifacts/sha256/aa/<hex>      content-addressed large payloads
  indexes/search.sqlite          derived full-text index (rebuildable)
  provenance/provenance.sqlite   durable sources, observations, conflicts, parse errors
  reports/*.md|json              import, coverage, duplicates, conflicts, privacy
  checksums.sha256               hashes of every produced file
```

The canonical archive is the event shards plus `provenance/provenance.sqlite`;
that database preserves source paths, machine/provider identity and per-event
source observations that are not recoverable from event lines. Back up and copy
it with the event shards and manifest. Search indexes, aggregates and reports
are derived and can be rebuilt. `convolith validate` verifies the ledger and its
checksum. The format is specified publicly:
[`spec/`](spec/README.md).

## Why

AI history fragments. The same conversation exists as a live session store, a
backup tarball of that store, an exported zip and a resume-fork of itself. Every
reader of those files came up with its own lossy answer: drop the tool output,
re-write ids, pick one branch, skip the records it didn't understand.

convolith takes the opposite default:

- **Nothing is dropped silently.** Every source record is imported, counted as a
  duplicate, counted as skipped with a reason, or counted as failed — and
  `convolith validate` checks the accounting identity
  `examined = imported + duplicate + skipped + failed`.
- **Provenance is not optional.** Every event carries the source, its position
  inside it and the parser that read it; multi-source events keep every
  observation.
- **Identity is deterministic.** Ids are pure functions of provider coordinates,
  so re-importing the same bytes changes nothing.
- **Conflicts are preserved.** Same identity, different content: both variants
  are kept and reported, never silently resolved.
- **Reasoning is never invented.** Only reasoning text literally present in the
  source is emitted; encrypted chain-of-thought stays an opaque blob.
- **Redaction is per-event and counted**, and the originals on disk are never
  modified.

## Status

Early. Version `0.1.0`, canonical `schema_version` 1. Interfaces will move
before 1.0.

### Support matrix

"Supported" means the parser has fixtures and acceptance tests in this
repository. Nothing in this table is claimed to be complete: agent session
formats are private, undocumented and change without notice.

| Source | Status | Notes |
|--------|--------|-------|
| Claude Code JSONL (`~/.claude/projects/**`) | supported |messages, tool calls/results, thinking (public/summary/opaque), subagent sidechains, compaction/summary records, interruptions, errors; bookkeeping record types are deliberately skipped and counted |
| Codex rollout JSONL (`~/.codex/sessions/**`) | supported | session headers incl. legacy shape, response items, reasoning summaries, encrypted reasoning kept opaque, function/custom/shell/web-search calls, `event_msg` kinds; turn contexts applied to following events |
| ChatGPT export (`conversations.json`) | supported | the `mapping` node graph is kept with its forks; no remote attachments are fetched |
| Generic JSON / JSON Lines | supported, lossy-by-design | last-resort role/content extraction for files that no other parser claims; unknown fields survive where the envelope allows |
| opencode | supported | file store (`storage/{session,message,part}/**`), `opencode.db` (read via a staged copy), `opencode export` bundles; unknown part kinds kept opaque |
| pi / Oh My Pi | supported | JSONL tree sessions (`~/.pi/agent/sessions/**`, `~/.omp/agent/**`); every branch imported, not just the active path |
| hermes | supported | Hermes Agent `state.db` (staged copy), session JSON logs and gateway transcripts |
| gemini_cli | supported | Gemini CLI `~/.gemini/tmp/*/chats/session-*.jsonl` (and legacy `.json`) |
| antigravity | supported | Antigravity CLI `~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl`; its SQLite/protobuf stores, truncated `transcript.jsonl` copies and agent work files are inventoried as unsupported with a reason |
| Cursor, Aider, Continue, Cline, SQLite chat stores, other agent stores | planned | not implemented; contribute a parser |

Platforms:

| Platform | Status |
|----------|--------|
| Linux | built and tested locally |
| Windows | native MSVC executable, real local stores, WSL2 and Windows OpenSSH validated; see `docs/POST_V1_BACKLOG.md` |
| macOS | expected to work; not built in CI, not verified |

Path handling, case sensitivity and archive extraction differ between platforms;
Linux is the only platform with first-hand testing.

## Install

```sh
cargo build --release            # target/release/convolith
# or, from a release artifact:
tar xzf convolith-<version>-x86_64-unknown-linux-gnu.tar.gz
```

Requires Rust 1.75+. SQLite is bundled; there is no runtime dependency.

## Use

```sh
convolith discover --local               # what is on this machine, and which parser sees it
convolith inspect ~/.codex/sessions/x.jsonl   # every parser's verdict and why
convolith import ~/.claude/projects -o ./canonical-ai-history
convolith import backup.tar.gz -o ./canonical-ai-history --resume
convolith validate ./canonical-ai-history    # exit 1 on any FAIL
convolith search ./canonical-ai-history "retry path"
convolith provenance ./canonical-ai-history ev_0123456789abcdef01234567
convolith inspect-event ./canonical-ai-history ev_0123456789abcdef01234567
convolith report ./canonical-ai-history
convolith rebuild-index ./canonical-ai-history
convolith parsers                       # list built-in parsers (authoritative for what is supported)

# find the well-known stores on this machine, WSL distros and SSH hosts, then import
convolith collect --local --wsl --ssh user@ssh-host.example -o ./canonical-ai-history
convolith collect --all-machines --apps claude,codex --dry-run   # list only, write nothing
```

Import flags: `--dry-run` (inspect without writing), `--resume` (skip sources
whose size/mtime/parser fingerprint is unchanged and whose last run completed),
`--secret-policy redact|preserve`, `--config <file>` (machine/project/repository
aliases and hard limits).

The default secret policy is `redact`: matched credential patterns in stored
content become `[REDACTED:<kind>]`, each event records the redaction, and
`reports/PRIVACY_AUDIT.md` shows counts only. The originals are never touched.
Use `preserve` when you want the canonical copy to keep the raw text and the
audit to only count it.

`collect` uses only known paths unless `--deep` (depth-4 scan under your home, never
a whole disk), and within them only the files the parsers read (one rule table in
`discover.rs` drives both local walking and the remote file list; SQLite databases bring
their `-wal`/`-shm`/`-journal` files). Provenance records `machine_id` as
`linux/<hash>` or `windows/<hash>` for this machine, `wsl/<distro>` and `ssh/<host>` for remotes, with the
original path on that machine. WSL/SSH data is fetched with three `sh -s` round trips
(store directories, matching file names, then `tar -T -` over exactly those names; nothing
is installed remotely), staged under `<output>/collect-staging`, imported, then
removed; `collect-state.json` records per-store fingerprints and `--resume` skips
unchanged stores (without it everything is re-collected; dedup keeps that idempotent). SSH uses your own `ssh` (BatchMode, 10 s timeout; keys/`~/.ssh/config`
apply) and the remote needs POSIX `sh` and `tar` (GNU/BSD). Exit status is non-zero only
if every requested machine failed.

## One file with all your history

Collect every machine and every web export into one archive, then export a single
chronological file. On Windows (PowerShell), the same commands work on Linux/macOS with
different paths.

```powershell
# 1. Windows + every WSL distro + an SSH host (uses your own OpenSSH config/keys)
convolith collect --local --wsl --ssh user@ssh-host.example -o C:\convolith\archive

# 2. Official web exports (ChatGPT, Claude, Gemini Takeout, Perplexity zips)
convolith import C:\Users\you\Downloads\EXPORTS\chatgpt-export.zip -o C:\convolith\archive
convolith import C:\Users\you\Downloads\EXPORTS\claude-export.zip  -o C:\convolith\archive

# 3. One provider-independent, globally chronological, deduplicated JSONL
convolith all C:\convolith\archive --output C:\convolith\convolith-all.jsonl   # or --stdout

# 4. Optional: one portable file holding the whole archive (provenance included)
convolith pack C:\convolith\archive --output C:\convolith\history.convolith
convolith unpack C:\convolith\history.convolith --output C:\convolith\restored
```

`convolith-all.jsonl` is the interchange file for a harness; `history.convolith` is the
backup. Re-running steps 1 and 2 is safe: duplicates are skipped and `collect --resume`
skips unchanged stores. Confirm `ssh user@ssh-host.example` works on its own first.
Check `convolith <command> --help` for the exact flags of your build.

## Known limitations

- Antigravity SQLite/protobuf stores (`antigravity-cli/conversations/*.db`, `.pb`,
  `.pbtxt`) have no published schema and are inventoried as unsupported, not decoded.
- Audit of 8 WSL Antigravity databases without `transcript_full.jsonl` found
  7 empty trajectories and 1 undecoded 136-step protobuf trajectory; 3 Windows
  databases without transcripts are empty. See [storage audit](docs/ANTIGRAVITY_STORAGE_AUDIT.md).
- Antigravity `history.jsonl` is not imported: prompts overlap transcripts,
  command records and repeated text lack a validated dedup mapping.
- Antigravity events carry no working directory and tool results are not linked to
  their calls.
- Collection reads only the files the supported parsers need (per-store rules in
  `discover.rs`), so unsupported-file counts for `~/.gemini`, `~/.hermes`, `~/.minimax`
  are smaller than with `convolith import PATH`, which inventories everything it is
  given (MiniMax bookkeeping, its runtime SQLite index, tool-output spills, skills and
  install files are reported there as unsupported, each with a reason).
- Local machine identity hashes the OS installation identifier (Windows
  `MachineGuid`, Linux `/etc/machine-id` with `/var/lib/dbus/machine-id` fallback)
  into `windows/<24 hex>` or `linux/<24 hex>`. Hostname/path changes do not change
  it; reinstalling the OS or regenerating its identifier does. Cloned OS images
  need distinct OS identifiers. The raw identifier is never stored in archives.
  An unavailable identifier is a reported local-machine failure, isolated from
  WSL/SSH collection. WSL remains `wsl/<distro>`, SSH `ssh/<host>`; these remote
  names remain coarse. Source rows also carry `platform`.
- Existing `linux`/`windows` archives need no rewrite. Old root-state keys are
  retained but not reused for the new identity: the first collection revisits
  those stores; source-level resume can still skip unchanged records.
  Plain reimport remains idempotent. Canonical event ids, source ids,
  observations and existing source/event machine fields remain intact;
  newly encountered sources use the stable id, and old generic machine records
  can coexist with new stable records. No historical
  generic id is guessed to belong to a particular computer.
- DeepSeek: only the DSH harness (`~/.dsh/sessions/**/session.v4.jsonl[.zstd]`) has a
  readable local history and is parsed. On this machine it exists only on the Windows
  side (`C:\Users\<u>\.dsh`), which `collect --local` on Linux/WSL does not reach;
  import it with `convolith import /home/user/.dsh/sessions`. The DeepSeek desktop
  Electron profile (`%APPDATA%/@deepseek-ai/dsh-desktop`: LevelDB, caches) holds no
  conversations. Per-chunk streaming data and provider `replayState` of assistant
  messages are not retained (the final content is).
- MiniMax: `messages.jsonl` and the compaction `snapshots/` of each session are parsed
  (same message ids collapse; tool results that differ between a snapshot and the live
  file are kept as conflicts). The runtime SQLite index mirrors those files and is not
  decoded.
- Native Windows MSVC, `--wsl` against example-distro, and SSH through Windows
  OpenSSH have been validated on real stores; measured results are in
  [`docs/POST_V1_BACKLOG.md`](docs/POST_V1_BACKLOG.md).
- Claude queue/bridge preambles in known project stores get a bounded 1 MiB
  detection retry. Run one plain collection after upgrading to revisit sources
  previously classified unsupported; unchanged-root resume skips that work.

## Safety

- **Read-only sources.** convolith never writes to, moves, renames or deletes the
  files it imports.
- **Never executes source content.** No tool call, script, shell command or
  attachment from your history is ever run. Archives are parsed, never extracted
  with a shell.
- **No network.** The binary makes no outbound connections; nothing is uploaded,
  and no remote attachment is fetched.
- **Bounded input.** File, record, artifact, staging, file-count and depth limits
  are finite by default, and archive extraction is bounded and path-checked.
- **Redaction never hides its own work:** counts are recorded, values are not
  copied into reports or logs.

See [`SECURITY.md`](SECURITY.md) for the threat model and its limits. There is no
sandboxing story for the *dataset* itself: it contains your history, so treat it
as sensitive and protect it like the originals.

## Prior art

convolith is not the first tool in this space and does not pretend to be. Format
knowledge for several parsers comes from
[xhluca/session-migrate](https://github.com/xhluca/session-migrate) (MIT): the
*semantics* of each record kind were ported, not the code. IronBridge (AGPL)
informed the architecture at a high level only; no code was copied. The full,
honest picture — including prior art treated as irrelevant and why — is in
[`docs/PRIOR_ART.md`](docs/PRIOR_ART.md).

## Documentation

| Path | Contents |
|------|----------|
| [`spec/README.md`](spec/README.md) | canonical format v1: layout, framing, ids, timestamps, provenance, dedup/conflict, redaction |
| [`spec/*.schema.json`](spec) | JSON Schema 2020-12 for event, conversation, artifact, provenance |
| [`spec/examples/`](spec/examples) | examples, validated against the schemas |
| [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md) | prior art and what was taken from it |
| [`docs/SESSION_MIGRATE_MAPPING.md`](docs/SESSION_MIGRATE_MAPPING.md) | per-record mapping decisions |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | build, test and parser contributor guide |
| [`SECURITY.md`](SECURITY.md) | threat model and limits |
| [`CHANGELOG.md`](CHANGELOG.md) | changes per release |

## License

MIT — see [`LICENSE`](LICENSE).
