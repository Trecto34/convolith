# Threat model

convolith reads the most sensitive files on a machine: full agent transcripts,
tool calls with command output, credentials that were pasted into a prompt,
images, and backups of all of it. This document states what the tool defends
against, what it explicitly does not, and where the residual risk sits.

## Assets

| Asset | Where it lives |
|-------|----------------|
| Original session stores, exports, backups | wherever the operator points the tool; convolith only reads |
| Canonical dataset | `-o <dir>`; contains the same content plus provenance |
| Provenance ledger | `provenance/provenance.sqlite`; paths, machines, ids, timestamps |
| Reports | `reports/*.md|json`; counts by default, no secret values |

## Trust boundaries

1. **Untrusted input: the source files.** Session JSONL, exports, archives and
   databases may be attacker-influenced (a chat log is attacker-controllable
   text; a downloaded backup is an untrusted archive).
2. **Trusted: the operator's invocation.** The output directory, config file and
   CLI flags are operator-controlled.
3. **Trusted: the filesystem.** convolith assumes the OS enforces path permissions
   and that a plain `open`/`rename` behaves normally.
4. **No network boundary.** There is none: the binary makes no outbound
   connections, and no remote attachment is fetched, by design.

## Never-execute guarantee

- Source content is **data, never code**. No tool call, shell command, script,
  file path or attachment found in a transcript is executed, sourced, or passed
  to a shell.
- Archives are parsed in-process with Rust libraries (`zip`, `tar`, `flate2`,
  `zstd`), not extracted via `tar`, `unzip`, `7z` or any external command.
- SQLite input databases are **never opened in place**. The file (plus any
  `-wal`/`-journal`) is copied into the private staging directory and the *copy*
  is opened there, so WAL replay can happen without touching the original. No
  statement is ever issued against a source database.
- The only SQLite file this tool writes is the provenance ledger inside the
  output dataset.
- `convolith import` never writes to, moves, renames or deletes the files it reads.

## Defenses

| Threat | Mitigation |
|--------|------------|
| Zip-slip / path traversal from archive members | Entry names are sanitized and joined to a per-archive staging directory; absolute paths, `..` components and drive prefixes are rejected; rejected entries are counted (`archive_entries_rejected`) and reported, not silently ignored. |
| Archive / decompression bomb | Finite defaults: max file, record, artifact, staging, file-count and depth limits; extraction is bounded per archive and staging is deleted after use. |
| Memory exhaustion from a hostile record | Records are read through a bounded line reader; an over-long record is refused and counted, never buffered to completion. Nothing accumulates the whole corpus in memory: events stream to the shard writer one record at a time. |
| Sparse / oversized files, huge trees | File-count and depth limits, skip-lists for binary/irrelevant extensions, per-file size limit. |
| Secret leakage into logs and reports | Reports and the manifest carry **counts only**; the redaction marker replaces the value in stored content; `timestamp_original` is the only source text kept verbatim in an otherwise-non-content field. |
| Silently lost history | The accounting identity is checked by `validate`; unknown record types become `opaque` events rather than being dropped. |
| Corrupted output from an interrupted run | Shards and checksums are written to a temp file and renamed; an abandoned run leaves no partial shard. A half-written dataset is detectable by `validate`. |
| Accidental overwrite of unrelated data | Data is only ever written under `-o <dir>`; `is_dataset` gates dataset operations. |

## Limits and residual risk (be honest)

- **The dataset is as sensitive as the sources.** It is a copy of your history
  plus better metadata. convolith does not encrypt it, does not set restrictive
  permissions on it, and does not redact paths, machine names, project names or
  branch names. Protect it the way you protect the originals.
- **Redaction is best-effort pattern matching, not a guarantee.** It recognises
  a fixed list of credential shapes (provider key prefixes, private-key
  headers, JWTs, `Authorization:`/`Cookie:` headers, `password=`-style
  assignments, `user:pass@` URLs). A truncated, obfuscated, base64-encoded or
  unknown-format secret will not match. `preserve` keeps raw text on purpose.
  Treat both policies as damage limitation, not as sanitization.
- **Redaction never rewrites the originals.** If a key was in the transcript, it
  is still there; convolith does not remediate the source, and does not tell you
  which of your files contain live credentials.
- **`source_sha256`, `source_path`, `first_seen` and the ledger are metadata
  leaks.** They reveal file layout, machine names and import timing. They are
  intentional — without them provenance is not auditable — but they mean a
  redacted dataset is not automatically safe to share.
- **No sandbox.** Parsing happens with the same privileges as the invoking user.
  A memory-safety bug in a parser or a decompression library is the realistic
  worst case; that is why the input limits exist and why parsers are expected to
  be streaming and bounded.
- **No cryptographic integrity of the dataset over time.** `checksums.sha256`
  detects corruption and accidental modification, not tampering by someone who
  can rewrite the dataset. Anything that can edit `data/events/` can also edit
  the checksums.
- **Detection is not support.** A parser reporting `strong` or `certain`
  detection is claiming the *format*, not that extraction was complete. Every
  parser records what it skips; read the import report before trusting a count.
- **No hostile-input fuzzing in CI yet.** The acceptance suite covers malformed
  and truncated inputs, but the parsers have not been fuzzed continuously.
- **Platform gaps.** Windows is built and tested in CI only; macOS is untested.
  Path-normalization differences are the most likely source of surprises.

## Reporting a vulnerability

Open a private security advisory on the repository, or contact the maintainers
directly if that is not available. Include the affected command, the smallest
input that reproduces it, and the observed versus expected behavior.

Please do not include live credentials, real conversation content, or personal
paths in a report: reduce the input to a synthetic fixture that triggers the
same behavior. That also gives us a regression test for free.

We will confirm receipt, aim to reproduce within a few days, and credit the
reporter unless asked not to. There is no bug bounty and no coordinated-release
service-level agreement — this is an early, maintainer-run project.