# Known limitations

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
