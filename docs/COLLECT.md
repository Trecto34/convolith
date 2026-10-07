# `convolith collect`

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
