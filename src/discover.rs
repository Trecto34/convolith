//! Recursive discovery: from input paths to parseable sources.
//!
//! Discovery is conservative. A file name is never evidence on its own: every
//! candidate is offered to the parser registry, and a file nothing recognizes is
//! recorded as unsupported rather than guessed at. Archives are expanded into a
//! tool-owned staging directory (never in place), with their ancestry kept, so
//! `server-backup.tar.gz!/home/u/.codex/sessions/x.jsonl` stays two levels deep
//! in provenance.

use crate::archive::{self, ArchiveKind};
use crate::config::Config;
use crate::scratch::Scratch;
use crate::source::{Probe, Source};
use crate::timeutil;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Bytes read for detection. Large enough for a JSON header, small enough that
/// probing a 4 GiB file stays cheap.
pub const PROBE_BYTES: usize = 16 * 1024;
const MAX_SIBLINGS: usize = 64;

#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    pub max_depth: usize,
    pub max_files: u64,
    pub skip_extensions: Vec<String>,
    /// Expand archives into staging and yield their members.
    pub expand_archives: bool,
    /// Follow directory symlinks during the walk (off by default: a link can
    /// point anywhere, including outside the tree the operator named).
    pub follow_symlinks: bool,
    pub max_file_bytes: u64,
    pub machine_id: Option<String>,
    pub platform_hint: Option<String>,
    /// Directory names never descended into (browser/Electron caches).
    pub skip_dirs: Vec<String>,
    /// Collection mode: inside a known store, only descend into / emit the files
    /// its rules name (see [`path_wanted`]). Off for `import PATH`, which
    /// inventories everything it is pointed at.
    pub known_stores_only: bool,
    /// `(staging_root, original_root)`: files staged from another machine keep
    /// their original path as display path (and so in source identity).
    pub path_map: Option<(PathBuf, String)>,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        DiscoverOptions {
            max_depth: 48,
            max_files: 2_000_000,
            skip_extensions: Vec::new(),
            expand_archives: true,
            follow_symlinks: false,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            machine_id: None,
            platform_hint: None,
            skip_dirs: Vec::new(),
            known_stores_only: false,
            path_map: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DiscoverStats {
    pub files_seen: u64,
    pub files_skipped: u64,
    pub bytes_seen: u64,
    pub dirs_seen: u64,
    pub archives_expanded: u64,
    pub archive_entries: u64,
    pub archive_entries_rejected: u64,
    pub errors: Vec<String>,
    pub failed_sources: Vec<serde_json::Value>,
    /// Sources that were found and inventoried but deliberately not
    /// interpreted (`source_id`, `display_path`, `size`, `note`).
    pub unresolved_sources: Vec<serde_json::Value>,
}

/// Walk one input root, invoking `on_source` for every file worth probing.
///
/// `on_archive_entry` lets the caller decide whether an archive should be
/// expanded at all (the importer only expands what could contain sessions); it
/// is called with the archive path before expansion.
pub fn walk_root(
    root: &Path,
    opts: &DiscoverOptions,
    cfg: &Config,
    scratch_parent: &Path,
    on_source: &mut dyn FnMut(&Source, &Probe) -> Result<()>,
    stats: &mut DiscoverStats,
) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Windows canonicalization adds a verbatim prefix. Use the same spelling
    // for the staging root so strip_prefix can recover the remote path.
    let mut opts = opts.clone();
    if let Some((stage, _)) = &mut opts.path_map {
        *stage = stage.canonicalize().unwrap_or_else(|_| stage.clone());
    }
    if !root.exists() {
        anyhow::bail!("input path does not exist: {}", root.display());
    }
    if root.is_file() {
        emit_file(
            &root,
            &root,
            &[],
            &opts,
            cfg,
            scratch_parent,
            stats,
            on_source,
        )?;
        return Ok(());
    }
    walk_dir(
        &root,
        &root,
        &[],
        0,
        &opts,
        cfg,
        scratch_parent,
        on_source,
        stats,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    root: &Path,
    dir: &Path,
    chain: &[String],
    depth: usize,
    opts: &DiscoverOptions,
    cfg: &Config,
    scratch_parent: &Path,
    on_source: &mut dyn FnMut(&Source, &Probe) -> Result<()>,
    stats: &mut DiscoverStats,
) -> Result<()> {
    if depth > opts.max_depth {
        stats
            .errors
            .push(format!("depth limit reached at {}", dir.display()));
        return Ok(());
    }
    stats.dirs_seen += 1;
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            stats
                .errors
                .push(format!("cannot read {}: {e}", dir.display()));
            return Ok(());
        }
    };
    let mut entries: Vec<PathBuf> = Vec::new();
    for e in rd {
        match e {
            Ok(e) => entries.push(e.path()),
            Err(err) => stats
                .errors
                .push(format!("entry error in {}: {err}", dir.display())),
        }
    }
    // Deterministic order: the same tree must produce the same import twice.
    entries.sort();
    let names: Vec<String> = entries
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    if crate::leveldb::is_leveldb(&names) {
        inventory_leveldb(dir, chain, scratch_parent, stats);
        return Ok(());
    }
    for path in entries {
        if stats.files_seen >= opts.max_files {
            stats
                .errors
                .push(format!("file limit {} reached; stopping", opts.max_files));
            return Ok(());
        }
        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                stats.errors.push(format!("stat {}: {e}", path.display()));
                stats.files_skipped += 1;
                continue;
            }
        };
        if md.file_type().is_symlink() {
            // A symlink is recorded as skipped, never followed: it can escape
            // the tree the operator named, and the same bytes are reachable
            // through their real path.
            stats.files_skipped += 1;
            continue;
        }
        if opts.known_stores_only && !path_wanted(&path, md.is_dir()) {
            continue;
        }
        if md.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if opts.skip_dirs.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                stats.files_skipped += 1;
                continue;
            }
            walk_dir(
                root,
                &path,
                chain,
                depth + 1,
                opts,
                cfg,
                scratch_parent,
                on_source,
                stats,
            )?;
            continue;
        }
        if !md.is_file() {
            stats.files_skipped += 1;
            continue;
        }
        emit_file(
            &path,
            root,
            chain,
            opts,
            cfg,
            scratch_parent,
            stats,
            on_source,
        )?;
    }
    Ok(())
}

/// Whether collection should visit `path` (a descendant of a store root), by the
/// per-store file rules in [`STORES`]. The same rules generate the remote file
/// listing, so local and remote collection select the same files.
///
/// * inside a store with [`Wanted::Only`] rules: only matching files (plus the
///   SQLite `-wal`/`-shm`/`-journal` companions of a matching `.db`) and the
///   directories that lead to them;
/// * inside an [`Wanted::Inventory`] store, and anywhere outside the known
///   stores: wanted;
/// * under the dot-directory of a rule-bound store but outside its roots
///   (`~/.gemini/history`, `~/.hermes/hermes-agent`): not wanted.
pub fn path_wanted(path: &Path, is_dir: bool) -> bool {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let mut in_store = false;
    for st in STORES {
        let rel: Vec<&str> = st.rel.split('/').collect();
        if comps.len() < rel.len() {
            continue;
        }
        for at in 0..=comps.len() - rel.len() {
            if comps[at..at + rel.len()] == rel[..] {
                in_store = true;
                if st.accepts(&comps[at + rel.len()..], is_dir) {
                    return true;
                }
            }
        }
    }
    if in_store {
        return false;
    }
    // A directory on the way to a store root (`~/.gemini` for `.gemini/tmp`).
    let ancestor = |rel: &str| {
        let r: Vec<&str> = rel.split('/').collect();
        (1..r.len()).any(|k| comps.ends_with(&r[..k]))
    };
    if is_dir && STORES.iter().any(|st| ancestor(st.rel)) {
        return true;
    }
    !STORES.iter().any(|st| {
        matches!(st.wanted, Wanted::Only(_))
            && st.rel.starts_with('.')
            && comps.contains(&st.rel.split('/').next().unwrap_or(""))
    })
}

/// `*` matches any run of characters inside one path segment.
fn glob_seg(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whole-path match; a `**` segment matches zero or more directories.
fn glob_path(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| glob_path(rest, &path[i..])),
        Some((p, rest)) => path
            .split_first()
            .is_some_and(|(s, tail)| glob_seg(p, s) && glob_path(rest, tail)),
    }
}

/// Could a directory at `path` (relative to the store root) hold a match?
fn glob_prefix(pat: &[&str], path: &[&str]) -> bool {
    match (pat.split_first(), path.split_first()) {
        (_, None) => !pat.is_empty(),
        (None, Some(_)) => false,
        (Some((&"**", _)), Some(_)) => true,
        (Some((p, rest)), Some((s, tail))) => glob_seg(p, s) && glob_prefix(rest, tail),
    }
}

const SQLITE_EXT: &[&str] = &[".db", ".sqlite", ".sqlite3", ".vscdb"];
/// Sidecar files SQLite keeps next to a database; needed to read it faithfully.
pub const SQLITE_COMPANIONS: &[&str] = &["-wal", "-shm", "-journal"];

/// `state.db-wal` -> `state.db`, only for a real SQLite database name.
fn sqlite_base(name: &str) -> Option<&str> {
    SQLITE_COMPANIONS
        .iter()
        .find_map(|c| name.strip_suffix(c))
        .filter(|b| SQLITE_EXT.iter().any(|e| b.ends_with(e)))
}

/// Whether `name` is a SQLite database file name (companions are looked up next to it).
pub fn is_sqlite_name(name: &str) -> bool {
    SQLITE_EXT.iter().any(|e| name.ends_with(e))
}

/// A LevelDB directory is one unresolved source, not a pile of binary files
/// for the parsers to guess at. Inspection runs on a staged copy.
fn inventory_leveldb(
    dir: &Path,
    chain: &[String],
    scratch_parent: &Path,
    stats: &mut DiscoverStats,
) {
    let display = display_path(dir, chain);
    let report =
        Scratch::create(scratch_parent, "ldb").and_then(|s| crate::leveldb::inspect(dir, s.path()));
    let (size, note) = match report {
        Ok(r) => {
            stats.files_seen += r.files as u64;
            stats.bytes_seen += r.bytes;
            (r.bytes, r.note)
        }
        Err(e) => (
            0,
            format!("LevelDB store could not be inspected: {e}; unresolved"),
        ),
    };
    stats.unresolved_sources.push(serde_json::json!({
        "source_id": crate::dedup::source_identity(&display, chain),
        "display_path": display,
        "size": size,
        "note": note,
    }));
}

#[allow(clippy::too_many_arguments)]
fn emit_file(
    path: &Path,
    root: &Path,
    chain: &[String],
    opts: &DiscoverOptions,
    cfg: &Config,
    scratch_parent: &Path,
    stats: &mut DiscoverStats,
    on_source: &mut dyn FnMut(&Source, &Probe) -> Result<()>,
) -> Result<()> {
    stats.files_seen += 1;
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            stats.errors.push(format!("stat {}: {e}", path.display()));
            stats.files_skipped += 1;
            return Ok(());
        }
    };
    let size = md.len();
    if size == 0 {
        stats.files_skipped += 1;
        return Ok(());
    }
    if size > opts.max_file_bytes {
        stats.files_skipped += 1;
        stats.errors.push(format!(
            "skipped {} ({size} bytes exceeds per-file limit)",
            path.display()
        ));
        return Ok(());
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let skip = opts
        .skip_extensions
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&ext));
    let head = match read_head_checked(path, PROBE_BYTES) {
        Ok(head) => head,
        Err(e) => {
            let display = mapped_display_path(path, chain, opts);
            let message = format!("cannot read source: {e}");
            stats.errors.push(format!("{display}: {message}"));
            stats.failed_sources.push(serde_json::json!({
                "source_id": crate::dedup::source_identity(&display, chain),
                "display_path": display,
                "note": message,
            }));
            return Ok(());
        }
    };
    // Archive detection wins over the skip list: `.zip` is skipped as a *file*
    // but still expanded as a container.
    let kind = crate::archive::classify(&head, &path.to_string_lossy());
    if let Some(kind) = kind {
        if opts.expand_archives && size <= opts.max_file_bytes {
            expand_archive(
                path,
                root,
                kind,
                chain,
                opts,
                cfg,
                scratch_parent,
                stats,
                on_source,
            )?;
            return Ok(());
        }
    }
    if skip {
        stats.files_skipped += 1;
        return Ok(());
    }
    stats.bytes_seen += size;
    let display = mapped_display_path(path, chain, opts);
    let full_path = path.to_string_lossy().into_owned();
    let (machine_id, platform) = machine_for(cfg, path, opts);
    let source = Source {
        source_id: crate::dedup::source_identity(&display, chain),
        display_path: display.clone(),
        container_chain: chain.to_vec(),
        read_path: path.to_path_buf(),
        inner_path: chain.last().cloned(),
        size,
        mtime: md.modified().ok().and_then(timeutil::from_system_time),
        sha256: None,
        machine_id,
        platform,
        provider_label: None,
        application_label: None,
        parser_id: None,
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let probe = Probe {
        path: path.to_path_buf(),
        parent_name: path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(str::to_string),
        siblings: siblings_of(path),
        size,
        head: String::from_utf8_lossy(&head).into_owned(),
        head_bytes: head,
        is_dir: false,
        rel_path: rel.clone(),
        full_path,
    };
    let _ = rel;
    on_source(&source, &probe)
}

#[allow(clippy::too_many_arguments)]
fn expand_archive(
    path: &Path,
    root: &Path,
    kind: ArchiveKind,
    chain: &[String],
    opts: &DiscoverOptions,
    cfg: &Config,
    scratch_parent: &Path,
    stats: &mut DiscoverStats,
    on_source: &mut dyn FnMut(&Source, &Probe) -> Result<()>,
) -> Result<()> {
    // A scratch directory per archive keeps extraction bounded and guarantees
    // cleanup: it is removed when this function returns, after every member has
    // been offered to the registry.
    let scratch = match Scratch::create(scratch_parent, "ar") {
        Ok(s) => s,
        Err(e) => {
            stats
                .errors
                .push(format!("staging for {}: {e}", path.display()));
            return Ok(());
        }
    };
    let limits = cfg.limits.archive();
    // `walk_archive` appends the container's own name to the chain it is
    // given, so the ancestors are passed without it and each yielded entry
    // receives the full `outer.tgz!/inner/path` ancestry.
    stats.archives_expanded += 1;

    let mut collected: Vec<(archive::ExtractedEntry, Vec<String>)> = Vec::new();
    // Staged bytes stay valid until `scratch` is dropped at the end of this
    // function, so the entries can be replayed after the walk returns.
    let walk = archive::walk_archive(
        path,
        kind,
        scratch.path(),
        chain.len() as u32,
        &limits,
        true,
        chain,
        &mut |entry, member_chain| {
            collected.push((entry.clone_entry(), member_chain.to_vec()));
            Ok(())
        },
    );
    match walk {
        Ok(summary) => {
            stats.archive_entries += summary.entries_extracted;
            stats.archive_entries_rejected += summary.entries_skipped;
            for note in summary.notes.iter().take(20) {
                stats.errors.push(format!("{}: {note}", path.display()));
            }
            for (name, why) in &summary.entries_failed {
                let display = if name.is_empty() {
                    display_path(path, chain)
                } else {
                    format!("{}!/{}", display_path(path, chain), name)
                };
                let message = format!("cannot read archive entry {display}: {why}");
                stats.errors.push(message.clone());
                stats.files_skipped += 1;
                stats.failed_sources.push(serde_json::json!({
                    "source_id": crate::dedup::source_identity(&display, chain),
                    "display_path": display,
                    "status": "failed",
                    "records_examined": 0,
                    "records_imported": 0,
                    "records_duplicate": 0,
                    "records_skipped": 0,
                    "records_failed": 0,
                    "note": message,
                }));
            }
        }
        Err(e) => {
            let message = format!("cannot expand {}: {e}", path.display());
            stats.errors.push(message.clone());
            stats.files_skipped += 1;
            stats.failed_sources.push(serde_json::json!({
                "source_id": crate::dedup::source_identity(&display_path(path, chain), chain),
                "display_path": display_path(path, chain),
                "status": "failed",
                "records_examined": 0,
                "records_imported": 0,
                "records_duplicate": 0,
                "records_skipped": 0,
                "records_failed": 0,
                "note": message,
            }));
            return Ok(());
        }
    }

    for (entry, member_chain) in collected {
        stats.files_seen += 1;
        let rel = entry.inner_path.clone();
        let head = read_head(&entry.staged_path, PROBE_BYTES);
        let display = format!("{}!/{}", display_path(path, chain), rel.replace('\\', "/"));
        let (machine_id, platform) = machine_for(cfg, path, opts);
        let source = Source {
            source_id: crate::dedup::source_identity(&display, &member_chain),
            display_path: display.clone(),
            container_chain: member_chain.clone(),
            read_path: entry.staged_path.clone(),
            inner_path: Some(rel.clone()),
            size: entry.size,
            mtime: entry.mtime,
            sha256: None,
            machine_id,
            platform,
            provider_label: None,
            application_label: None,
            parser_id: None,
        };
        let probe = Probe {
            path: entry.staged_path.clone(),
            parent_name: Path::new(&rel)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(str::to_string),
            siblings: Vec::new(),
            size: entry.size,
            head: String::from_utf8_lossy(&head).into_owned(),
            head_bytes: head,
            is_dir: false,
            rel_path: rel,
            full_path: display.clone(),
        };
        let _ = root;
        on_source(&source, &probe)?;
    }
    Ok(())
}

fn machine_for(
    cfg: &Config,
    path: &Path,
    opts: &DiscoverOptions,
) -> (Option<String>, Option<String>) {
    if let Some(id) = &opts.machine_id {
        return (Some(id.clone()), opts.platform_hint.clone());
    }
    let s = path.to_string_lossy();
    if let Some((id, platform)) = crate::identity::resolve_machine(cfg, &s) {
        return (Some(id), platform.or_else(|| opts.platform_hint.clone()));
    }
    (None, opts.platform_hint.clone())
}

fn display_path(path: &Path, chain: &[String]) -> String {
    if chain.is_empty() {
        path.to_string_lossy().into_owned()
    } else {
        format!(
            "{}!/{}",
            chain.join("!/"),
            path.file_name().and_then(|s| s.to_str()).unwrap_or("")
        )
    }
}

fn mapped_display_path(path: &Path, chain: &[String], opts: &DiscoverOptions) -> String {
    if let (Some((stage, orig)), true) = (&opts.path_map, chain.is_empty()) {
        if let Ok(rel) = path.strip_prefix(stage) {
            return format!(
                "{}/{}",
                orig.trim_end_matches('/'),
                rel.to_string_lossy().replace('\\', "/")
            );
        }
    }
    display_path(path, chain)
}

fn read_head(path: &Path, n: usize) -> Vec<u8> {
    read_head_checked(path, n).unwrap_or_default()
}

fn read_head_checked(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path)?
        .take(n as u64)
        .read_to_end(&mut head)?;
    Ok(head)
}

fn siblings_of(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Some(parent) = path.parent() else {
        return out;
    };
    let Ok(rd) = std::fs::read_dir(parent) else {
        return out;
    };
    for e in rd.flatten().take(MAX_SIBLINGS) {
        if let Some(n) = e.file_name().to_str() {
            out.push(n.to_string());
        }
    }
    out.sort();
    out
}

/// Test helper: build a probe for one file without walking.
pub fn probe_file(path: &Path) -> Result<Probe> {
    let md = std::fs::metadata(path)?;
    let head = read_head(path, PROBE_BYTES);
    Ok(Probe {
        path: path.to_path_buf(),
        parent_name: path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(str::to_string),
        siblings: siblings_of(path),
        size: md.len(),
        head: String::from_utf8_lossy(&head).into_owned(),
        head_bytes: head,
        is_dir: md.is_dir(),
        rel_path: path.to_string_lossy().into_owned(),
        full_path: path.to_string_lossy().into_owned(),
    })
}

/// A well-known per-user store: where an AI client keeps local history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRoot {
    pub app: &'static str,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOs {
    Windows,
    /// Linux, including WSL: a WSL distro is a Linux machine; the Windows side
    /// is reached only when the operator names `/mnt/c/...` explicitly.
    Linux,
}

/// Candidate store locations for `os`, resolved through `env` (so tests do not
/// depend on the real environment). Existence is not checked here.
///
/// Locations only *nominate* directories. Nothing here treats a file name such
/// as `chat.db` as evidence: whatever lives under a root still has to be
/// claimed by a parser.
pub fn local_candidates(os: LocalOs, env: &dyn Fn(&str) -> Option<String>) -> Vec<LocalRoot> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut out: Vec<LocalRoot> = Vec::new();
    let mut add = |app: &'static str, p: PathBuf| {
        if !out.iter().any(|r| r.path == p) {
            out.push(LocalRoot { app, path: p });
        }
    };
    let (home, config, data, state) = match os {
        LocalOs::Windows => {
            let Some(home) = get("USERPROFILE") else {
                return out;
            };
            let roaming = get("APPDATA").unwrap_or_else(|| home.join("AppData").join("Roaming"));
            let local = get("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData").join("Local"));
            (home, roaming, local.clone(), local)
        }
        LocalOs::Linux => {
            let Some(home) = get("HOME") else {
                return out;
            };
            (
                home.clone(),
                get("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
                get("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local").join("share")),
                get("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local").join("state")),
            )
        }
    };
    if let Some(d) = get("CLAUDE_CONFIG_DIR") {
        add("claude-code", d.join("projects"));
    }
    if let Some(d) = get("CODEX_HOME") {
        add("codex", d.join("sessions"));
        add("codex", d.join("archived_sessions"));
    }
    for st in stores_for(os) {
        let base = match st.base {
            Base::Home => &home,
            Base::Config => &config,
            Base::Data => &data,
            Base::State => &state,
        };
        add(
            st.app,
            st.rel.split('/').fold(base.clone(), |p, c| p.join(c)),
        );
    }
    out
}

/// Where a known store hangs: `$HOME`, or an XDG-style (Windows: AppData) dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Home,
    /// `$XDG_CONFIG_HOME` / `%APPDATA%`.
    Config,
    /// `$XDG_DATA_HOME` / `%LOCALAPPDATA%`.
    Data,
    /// `$XDG_STATE_HOME` / `%LOCALAPPDATA%`.
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Only {
    Any,
    Windows,
    Posix,
}

/// Which files of a store collection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// No decoder reads this store: local collection still inventories what is
    /// there (LevelDB etc. as unresolved), remote collection copies nothing.
    Inventory,
    /// Only files matching one of these patterns, relative to the store root.
    /// A segment may contain `*`; a `**` segment matches any number of
    /// directories. A matching SQLite database brings its `-wal`/`-shm`/
    /// `-journal` companions. An empty list means "found, nothing to read".
    Only(&'static [&'static str]),
}

/// One known store: `rel` is `/`-separated, relative to `base`.
#[derive(Debug, Clone, Copy)]
pub struct Store {
    pub app: &'static str,
    pub base: Base,
    pub rel: &'static str,
    only: Only,
    pub wanted: Wanted,
}

const fn st(app: &'static str, base: Base, rel: &'static str, only: Only, wanted: Wanted) -> Store {
    Store {
        app,
        base,
        rel,
        only,
        wanted,
    }
}

impl Store {
    /// The patterns, empty for an inventory-only store.
    pub fn patterns(&self) -> &'static [&'static str] {
        match self.wanted {
            Wanted::Only(p) => p,
            Wanted::Inventory => &[],
        }
    }

    /// `rest` is a path below the store root.
    fn accepts(&self, rest: &[&str], is_dir: bool) -> bool {
        let Wanted::Only(pats) = self.wanted else {
            return true;
        };
        let split = |p: &str| p.split('/').map(str::to_string).collect::<Vec<_>>();
        pats.iter().any(|p| {
            let owned = split(p);
            let pat: Vec<&str> = owned.iter().map(String::as_str).collect();
            if is_dir {
                return glob_prefix(&pat, rest);
            }
            if glob_path(&pat, rest) {
                return true;
            }
            // A SQLite companion rides along with its database.
            let Some((name, dir)) = rest.split_last() else {
                return false;
            };
            sqlite_base(name).is_some_and(|base| {
                let mut r = dir.to_vec();
                r.push(base);
                glob_path(&pat, &r)
            })
        })
    }
}

const JSONL: Wanted = Wanted::Only(&["**/*.jsonl"]);
const OPENCODE: Wanted = Wanted::Only(&["*.db", "storage/**/*.json"]);
const HERMES: Wanted = Wanted::Only(&[
    "state.db",
    "sessions/session_*.json",
    "sessions/*.jsonl",
    "profiles/*/state.db",
    "profiles/*/sessions/session_*.json",
    "profiles/*/sessions/*.jsonl",
]);

/// The single app -> candidate-path table, shared by every machine kind
/// (native Windows/Linux, WSL distros, SSH hosts). Paths only *nominate*
/// directories; parsers still decide what is a session.
pub const STORES: &[Store] = &[
    st(
        "claude-code",
        Base::Home,
        ".claude/projects",
        Only::Any,
        JSONL,
    ),
    st(
        "claude-code",
        Base::Config,
        "claude/projects",
        Only::Posix,
        JSONL,
    ),
    st("codex", Base::Home, ".codex/sessions", Only::Any, JSONL),
    st(
        "codex",
        Base::Home,
        ".codex/archived_sessions",
        Only::Any,
        JSONL,
    ),
    st("opencode", Base::Data, "opencode", Only::Posix, OPENCODE),
    st(
        "opencode",
        Base::Home,
        ".local/share/opencode",
        Only::Windows,
        OPENCODE,
    ),
    st("pi", Base::Home, ".pi/agent/sessions", Only::Any, JSONL),
    st("omp", Base::Home, ".omp/agent/sessions", Only::Any, JSONL),
    st("hermes", Base::Home, ".hermes", Only::Any, HERMES),
    // Protobuf conversation stores: nothing readable at this root.
    st(
        "antigravity",
        Base::Home,
        ".gemini/antigravity",
        Only::Any,
        Wanted::Only(&[]),
    ),
    st(
        "antigravity",
        Base::Home,
        ".gemini/antigravity-cli",
        Only::Any,
        Wanted::Only(&["brain/*/.system_generated/logs/transcript_full.jsonl"]),
    ),
    // `.project_root` is the sibling file that names a chat session's cwd.
    st(
        "gemini",
        Base::Home,
        ".gemini/tmp",
        Only::Any,
        Wanted::Only(&[
            "*/chats/session-*.jsonl",
            "*/chats/session-*.json",
            "*/.project_root",
        ]),
    ),
    // MiniMax Code: live context plus the compaction snapshots of each session.
    st(
        "minimax",
        Base::Home,
        ".minimax",
        Only::Any,
        Wanted::Only(&[
            "v2/sessions/**/messages.jsonl",
            "v2/sessions/**/snapshots/g*--ctx_*.jsonl",
        ]),
    ),
    // DeepSeek DSH: zstd-compressed event log per session.
    st(
        "deepseek",
        Base::Home,
        ".dsh",
        Only::Any,
        Wanted::Only(&[
            "sessions/*/*/session.v4.jsonl.zstd",
            "sessions/*/*/session.v4.jsonl",
        ]),
    ),
    // Electron desktop clients keep a profile directory; its LevelDB/IndexedDB
    // parts are inventoried as unresolved, never guessed at.
    st(
        "claude-desktop",
        Base::Config,
        "Claude",
        Only::Any,
        Wanted::Inventory,
    ),
    st(
        "chatgpt-desktop",
        Base::Config,
        "ChatGPT",
        Only::Any,
        Wanted::Inventory,
    ),
    st(
        "cursor",
        Base::Config,
        "Cursor/User/workspaceStorage",
        Only::Any,
        Wanted::Inventory,
    ),
    st(
        "claude-desktop",
        Base::Data,
        "Claude",
        Only::Any,
        Wanted::Inventory,
    ),
    st(
        "claude-desktop",
        Base::Data,
        "AnthropicClaude",
        Only::Windows,
        Wanted::Inventory,
    ),
    st(
        "claude-desktop",
        Base::State,
        "Claude",
        Only::Posix,
        Wanted::Inventory,
    ),
];

/// The store roots a discovered directory stands for: the directory itself when
/// it is a store root, or the store roots nested in it when it is the top-level
/// dot-directory a `--deep` scan found (`~/.gemini` -> `.gemini/tmp`, ...).
pub fn roots_for_hit(path: &str) -> Vec<(&'static Store, String)> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let base = path.trim_end_matches('/');
    let mut out: Vec<(&'static Store, String)> = Vec::new();
    for st in stores_for(LocalOs::Linux) {
        let rel: Vec<&str> = st.rel.split('/').collect();
        let root = if comps.ends_with(&rel) {
            Some(base.to_string())
        } else if rel.len() > 1 && comps.ends_with(&rel[..1]) && rel[0].starts_with('.') {
            Some(format!("{base}/{}", rel[1..].join("/")))
        } else {
            None
        };
        if let Some(r) = root {
            if !out.iter().any(|(s, p)| s.rel == st.rel && *p == r) {
                out.push((st, r));
            }
        }
    }
    out
}

pub fn stores_for(os: LocalOs) -> impl Iterator<Item = &'static Store> {
    STORES.iter().filter(move |s| match s.only {
        Only::Any => true,
        Only::Windows => os == LocalOs::Windows,
        Only::Posix => os == LocalOs::Linux,
    })
}

/// Top-level dot-directories under home that name a known app, for `--deep`.
pub fn deep_dir_names() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = STORES
        .iter()
        .filter(|s| s.base == Base::Home)
        .filter_map(|s| {
            let top = s.rel.split('/').next()?;
            (top.starts_with('.') && top != ".local").then_some((s.app, top))
        })
        .collect();
    v.dedup_by_key(|(_, d)| *d);
    v
}

/// Candidates that exist, with the filesystem injected so tests can fake it.
pub fn local_roots_with(
    os: LocalOs,
    env: &dyn Fn(&str) -> Option<String>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Vec<LocalRoot> {
    local_candidates(os, env)
        .into_iter()
        .filter(|r| is_dir(&r.path))
        .collect()
}

/// The current machine's candidates that exist as directories.
pub fn local_roots() -> Vec<LocalRoot> {
    let os = if cfg!(windows) {
        LocalOs::Windows
    } else {
        LocalOs::Linux
    };
    local_roots_with(os, &|k| std::env::var(k).ok(), &|p| p.is_dir())
}

/// Directory names that only ever hold browser/Electron caches.
pub fn local_skip_dirs() -> Vec<String> {
    [
        "Cache",
        "Code Cache",
        "GPUCache",
        "DawnGraphiteCache",
        "DawnWebGPUCache",
        "Crashpad",
        "node_modules",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
