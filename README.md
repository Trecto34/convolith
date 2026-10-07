<div align="center">

# convolith

**Every AI conversation you've ever had, in one canonical, provenance-first archive.**

Claude · Codex · OpenCode · Gemini · Antigravity · Hermes · Pi · MiniMax · DeepSeek · ChatGPT · Perplexity

Rust · no network · nothing executed · MIT

</div>

---

AI history is scattered across session stores, backups, exports and machines.
convolith collects it all, deduplicates without deleting, keeps conflicts, and
records where every event came from. Then it hands you **one chronological,
provider-independent JSONL** for whatever tool comes next.

```text
 Windows · WSL · SSH · web exports
                 │
            convolith collect / import
                 ▼
        canonical archive (source of truth)
                 │
     ┌───────────┴────────────┐
     ▼                        ▼
convolith all            convolith pack
convolith-all.jsonl      history.convolith
(harness interchange)    (portable backup)
```

## Quick start

```sh
cargo build --release                      # target/release/convolith (Rust 1.75+, SQLite bundled)

# 1. collect: known paths on this machine, every WSL distro, SSH hosts
convolith collect --local --wsl --ssh user@host -o ./archive

# 2. import official web exports (zip, folder, or bare JSON; multiple files supported)
convolith import ~/Downloads/chatgpt-export.zip ~/Downloads/claude-export.zip -o ./archive

# 3. one chronological, deduplicated file
convolith all ./archive --output convolith-all.jsonl      # or --stdout

# 4. check it
convolith validate ./archive
```

Re-running is safe: duplicates are skipped, and `--resume` skips unchanged stores.

## Commands

| Command | Does |
|---|---|
| `collect` | Auto-detect and import known stores: `--local --wsl --ssh HOST --all-machines --apps --dry-run --resume --deep` ([details](docs/COLLECT.md)) |
| `import PATH... -o DIR` | Manual or forensic import of one or more files, folders, tarballs or export zips |
| `all ARCHIVE` | Single chronological JSONL (`convolith.all/v1`) with `--output FILE` or `--stdout` |
| `pack` / `unpack` | Whole archive ⇄ one `history.convolith` file (provenance included) |
| `validate` | Verify checksums, provenance ledger and the import accounting identity |
| `discover` / `inspect` / `parsers` | See what exists, which parser claims it, and why |
| `search` / `provenance` / `inspect-event` / `report` / `rebuild-index` | Query and maintain an archive |

Import accepts one or more files or directories. An interactive terminal displays
a live progress bar during source import and dataset finalization.
Import flags: `--dry-run`, `--resume`, `--secret-policy redact|preserve` (default
`redact`; originals are never touched), `--config <file>`, `--no-progress`, `--progress`.

## Guarantees

- **Nothing dropped silently.** `examined = imported + duplicate + skipped + failed`, checked by `validate`.
- **Provenance on every event:** machine, original path, provider, parser, position.
- **Deterministic ids.** Re-importing the same bytes changes nothing.
- **Conflicts preserved,** never resolved. Dedup never uses text alone.
- **No invented data:** no made-up timestamps or reasoning; unknown formats are inventoried, not guessed.
- **Read-only, offline, inert:** sources are never modified, nothing is executed, no network.

## What it reads

| Source | Status |
|---|---|
| Claude Code, Codex, OpenCode, Pi / Oh My Pi, Hermes | supported |
| Gemini CLI, Antigravity (transcripts) | supported; Antigravity protobuf DBs not decoded |
| MiniMax / mcode, DeepSeek (DSH) | supported |
| ChatGPT, Claude (+ Design), Perplexity, Gemini Takeout web exports | supported, verified on real exports ([details](docs/WEB_EXPORTS.md)) |
| Generic JSON / JSONL | last-resort, lossy by design |
| Cursor, Aider, Continue, Cline, other stores | planned |

`convolith parsers` is authoritative. Gaps and caveats: [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md).

| Platform | Status |
|---|---|
| Linux | built and tested |
| Windows | native MSVC build, WSL2 and OpenSSH validated |
| macOS | expected to work; unverified |

## Archive layout

```text
archive/
  manifest.json                  metadata, shard index, run log
  data/events/part-*.jsonl.zst   append-only canonical events (authoritative)
  provenance/provenance.sqlite   sources, observations, conflicts (authoritative)
  artifacts/sha256/              content-addressed payloads
  indexes/ data/aggregates/      derived, rebuildable
  reports/                       import, coverage, duplicates, conflicts, privacy
  checksums.sha256
```

The event shards **and** `provenance.sqlite` are the canonical archive. Back them
up together (or use `pack`). Format spec: [`spec/`](spec/README.md).

## Safety

The dataset contains your history: protect it like the originals. Input limits,
path checks and zip-bomb guards apply to every archive. Threat model:
[`SECURITY.md`](SECURITY.md).

## Docs

| | |
|---|---|
| [`spec/README.md`](spec/README.md) | canonical format v1 |
| [`spec/all-export.md`](spec/all-export.md) | `convolith all` schema, ordering, limits |
| [`docs/COLLECT.md`](docs/COLLECT.md) | collect internals: discovery, WSL, SSH, resume |
| [`docs/WEB_EXPORTS.md`](docs/WEB_EXPORTS.md) | web export formats and verification status |
| [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) | known gaps |
| [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md) | prior art ([session-migrate](https://github.com/xhluca/session-migrate), IronBridge) |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) · [`CHANGELOG.md`](CHANGELOG.md) | contributing, releases |

Early: `0.x`, interfaces may change before 1.0. MIT, see [`LICENSE`](LICENSE).
