//! `convolith collect`: find the well-known AI stores on this machine, its WSL
//! distros and SSH hosts, stage the remote ones, and feed everything through the
//! normal importer (so canonical archive, dedup and provenance are unchanged).
//!
//! Remote machines are reached only through the system `wsl.exe` / `ssh`
//! binaries behind the [`Runner`] trait; a POSIX `sh` script is piped to them
//! over stdin (nothing is installed) and a tar stream comes back.

use crate::config::Config;
use crate::dataset::write_atomic;
use crate::discover::{self, LocalOs, LocalRoot};
use crate::importer::{ImportOptions, Importer};
use crate::secrets::SecretPolicy;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const STATE_FILE: &str = "collect-state.json";
const STAGING_DIR: &str = "collect-staging";
const COMPLETE_MARK: &str = ".convolith-complete";
/// Pre-rename marker, still honoured so an interrupted `--resume` keeps its staging.
const LEGACY_COMPLETE_MARK: &str = ".aichive-complete";

// ---- command execution ------------------------------------------------------

pub struct Output {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs an external program with `stdin`. `Err` means it could not be started.
pub trait Runner {
    fn run(&self, prog: &str, args: &[String], stdin: &[u8]) -> Result<Output>;
}

pub struct SysRunner;

impl Runner for SysRunner {
    fn run(&self, prog: &str, args: &[String], stdin: &[u8]) -> Result<Output> {
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot start {prog}"))?;
        // Scripts are small (< pipe buffer), so writing before reading is safe.
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(stdin);
        }
        let o = child.wait_with_output()?;
        Ok(Output {
            ok: o.status.success(),
            stdout: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        })
    }
}

// ---- machines ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Local,
    Wsl(String),
    Ssh(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub app: String,
    /// Absolute path on the machine (POSIX for WSL/SSH).
    pub path: String,
}

#[derive(Debug)]
pub struct Machine {
    pub kind: Kind,
    /// Provenance id: `windows/<hash>`, `linux/<hash>`, `wsl/example-distro`, `ssh/host-a`.
    pub id: String,
    /// Display label: `Windows`, `WSL/example-distro`, `SSH/host-a`.
    pub label: String,
    pub hits: Vec<Hit>,
    /// Remote machines: the exact files that will be transferred (absolute paths).
    pub files: Vec<String>,
    pub error: Option<String>,
}

pub fn machine(kind: Kind, os: LocalOs) -> Machine {
    let mut error = None;
    let (id, label) = match &kind {
        Kind::Local => {
            let label = if os == LocalOs::Windows {
                "Windows"
            } else {
                "Linux"
            };
            let id = match local_machine_key() {
                Ok(key) => local_machine_id(os, &key),
                Err(e) => {
                    error = Some(format!(
                        "cannot determine stable local machine identity: {e:#}"
                    ));
                    String::new()
                }
            };
            (id, label.into())
        }
        Kind::Wsl(d) => (format!("wsl/{d}"), format!("WSL/{d}")),
        Kind::Ssh(h) => {
            let host = h.rsplit('@').next().unwrap_or(h);
            (format!("ssh/{host}"), format!("SSH/{host}"))
        }
    };
    Machine {
        kind,
        id,
        label,
        hits: Vec::new(),
        files: Vec::new(),
        error,
    }
}

/// Machine id of the machine running this process, `None` when it cannot be
/// determined. Used to stamp web-export imports (see `SourceParser::is_web_export`).
pub fn importing_machine_id() -> Option<String> {
    let os = if cfg!(windows) {
        LocalOs::Windows
    } else {
        LocalOs::Linux
    };
    Some(machine(Kind::Local, os).id).filter(|id| !id.is_empty())
}

/// Domain-separated hash; neither the OS identifier nor a filesystem path is
/// exposed in archive machine ids. Hostnames may change without changing this.
pub fn local_machine_id(os: LocalOs, key: &str) -> String {
    let platform = if os == LocalOs::Windows {
        "windows"
    } else {
        "linux"
    };
    let normalized = key.trim().to_ascii_lowercase();
    let hash = crate::id::id("", &["convolith-local-machine-v1", platform, &normalized]);
    format!("{platform}/{hash}")
}

fn local_machine_key() -> Result<String> {
    #[cfg(windows)]
    {
        let output = Command::new("reg.exe")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
                "/reg:64",
            ])
            .output()
            .context("reading Windows MachineGuid")?;
        if !output.status.success() {
            bail!("Windows MachineGuid registry query failed");
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let key = text
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next() == Some("MachineGuid") && fields.next() == Some("REG_SZ"))
                    .then(|| fields.next().map(str::to_string))
                    .flatten()
            })
            .context("Windows MachineGuid is missing")?;
        if key.chars().filter(|c| c.is_ascii_hexdigit()).count() != 32 {
            bail!("Windows MachineGuid is malformed");
        }
        Ok(key)
    }
    #[cfg(not(windows))]
    {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(key) = std::fs::read_to_string(path) {
                let key = key.trim();
                if key.len() == 32 && key.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(key.to_string());
                }
            }
        }
        bail!("no valid OS machine-id; expected /etc/machine-id or /var/lib/dbus/machine-id")
    }
}

/// `wsl.exe -l -q` prints UTF-16LE (with BOM); tolerate plain UTF-8 too.
pub fn parse_wsl_list(raw: &[u8]) -> Vec<String> {
    let text = if raw.len() >= 2 && (raw[0..2] == [0xFF, 0xFE] || raw.get(1) == Some(&0)) {
        let u: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(|l| l.trim().trim_matches('\0').to_string())
        // Docker's helper distros hold no user history.
        .filter(|l| !l.is_empty() && !l.starts_with("example-container"))
        .collect()
}

/// Concrete `Host` names from ssh config text; wildcard/negated patterns are
/// skipped. `Include` is not followed.
pub fn parse_ssh_hosts(config: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in config.lines() {
        let mut it = line.split_whitespace();
        if !it.next().is_some_and(|k| k.eq_ignore_ascii_case("host")) {
            continue;
        }
        for h in it {
            if !h.contains(['*', '?', '!']) && !out.iter().any(|o| o == h) {
                out.push(h.to_string());
            }
        }
    }
    out
}

// ---- remote scripts ---------------------------------------------------------

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// POSIX sh that prints `app<TAB>path` for each known store that exists.
pub fn list_script(deep: bool) -> String {
    let mut s = String::from(
        "H=${HOME:-}; [ -n \"$H\" ] || { echo 'HOME unset' >&2; exit 3; }\n\
C=${XDG_CONFIG_HOME:-$H/.config}; D=${XDG_DATA_HOME:-$H/.local/share}; S=${XDG_STATE_HOME:-$H/.local/state}\n\
p() { [ -d \"$2\" ] && printf '%s\\t%s\\n' \"$1\" \"$2\"; return 0; }\n",
    );
    for st in discover::stores_for(LocalOs::Linux) {
        let base = match st.base {
            discover::Base::Home => "$H",
            discover::Base::Config => "$C",
            discover::Base::Data => "$D",
            discover::Base::State => "$S",
        };
        s += &format!("p {} \"{base}/{}\"\n", st.app, st.rel);
    }
    if deep {
        let names: Vec<String> = discover::deep_dir_names()
            .iter()
            .map(|(_, d)| format!("-name {}", sq(d)))
            .collect();
        s += &format!(
            "find \"$H\" -maxdepth 4 \\( -name node_modules -o -name .cache -o -name .git \\) -prune -o -type d \\( {} \\) -print 2>/dev/null | while IFS= read -r d; do printf 'deep\\t%s\\n' \"$d\"; done\n",
            names.join(" -o ")
        );
    }
    s
}

/// POSIX sh that prints `F<TAB>path` for every file the store rules could want
/// under `roots`: one `find` per rule pattern (coarse, built from the same
/// patterns as [`discover::path_wanted`]) plus the SQLite companions of each
/// database it lists. The caller filters the output with `path_wanted`, so this
/// listing may over-match but never decides what is transferred.
pub fn files_script(roots: &[(&'static discover::Store, String)]) -> String {
    let prune: Vec<String> = discover::local_skip_dirs()
        .iter()
        .map(|d| format!("-name {}", sq(d)))
        .collect();
    let prune = format!("\\( -type d \\( {} \\) \\) -prune -o", prune.join(" -o "));
    let exts: Vec<String> = [".db", ".sqlite", ".sqlite3", ".vscdb"]
        .iter()
        .map(|e| format!("*{e}"))
        .collect();
    let comps: Vec<String> = discover::SQLITE_COMPANIONS.iter().map(|c| sq(c)).collect();
    let mut s = format!(
        "emit() {{ printf 'F\\t%s\\n' \"$1\"; case \"$1\" in {}) for x in {}; do [ -f \"$1$x\" ] && printf 'F\\t%s\\n' \"$1$x\"; done;; esac; return 0; }}\n",
        exts.join("|"),
        comps.join(" ")
    );
    for (store, root) in roots {
        for pat in store.patterns() {
            let segs: Vec<&str> = pat.split('/').collect();
            // The last segment is always matched (a bare `state.db` is a file, not a directory to enter).
            let lit = segs
                .iter()
                .take_while(|g| !g.contains('*'))
                .count()
                .min(segs.len() - 1);
            let start = format!("{}/{}", root.trim_end_matches('/'), segs[..lit].join("/"));
            let start = start.trim_end_matches('/');
            let rest = &segs[lit..];
            let (depth, cond) = if rest.contains(&"**") {
                // Unbounded depth: filter on the file name; `path_wanted` checks the rest.
                (String::new(), format!("-name {}", sq(rest[rest.len() - 1])))
            } else {
                (
                    format!("-mindepth {n} -maxdepth {n} ", n = rest.len()),
                    format!("-path {}", sq(&format!("{start}/{}", rest.join("/")))),
                )
            };
            s += &format!(
                "find {} {depth}{prune} -type f {cond} -print 2>/dev/null | while IFS= read -r f; do emit \"$f\"; done\n",
                sq(start)
            );
        }
    }
    s
}

/// POSIX sh streaming a tar of exactly `files` (absolute paths) to stdout. The
/// names are an explicit list read by `tar -T -` from a here-document, so no
/// directory is ever archived and the argument length is unbounded.
pub fn fetch_script(files: &[String]) -> String {
    let mut s = String::from("cd / && exec tar -cf - -T - <<'CONVOLITH_FILE_LIST'\n");
    for f in files {
        s += f.trim_start_matches('/');
        s.push('\n');
    }
    s + "CONVOLITH_FILE_LIST\n"
}

/// Command line for reaching a machine; `script` goes over stdin.
fn remote_cmd(kind: &Kind) -> (&'static str, Vec<String>) {
    match kind {
        Kind::Wsl(d) => (
            "wsl.exe",
            ["-d", d, "--", "sh", "-s"].map(String::from).to_vec(),
        ),
        Kind::Ssh(h) => (
            "ssh",
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                h,
                "sh",
                "-s",
            ]
            .map(String::from)
            .to_vec(),
        ),
        Kind::Local => unreachable!("local machines are read in place"),
    }
}

fn parse_hits(out: &[u8], apps: &[String]) -> Vec<Hit> {
    let deep = discover::deep_dir_names();
    let mut hits: Vec<Hit> = Vec::new();
    let text = String::from_utf8_lossy(out);
    for (app, path) in text.lines().filter_map(|l| l.split_once('\t')) {
        let app = if app == "deep" {
            let base = path.rsplit('/').next().unwrap_or("");
            match deep.iter().find(|(_, d)| *d == base) {
                Some((a, _)) => a.to_string(),
                None => continue,
            }
        } else {
            app.to_string()
        };
        let covered = hits
            .iter()
            .any(|h| path == h.path || path.starts_with(&format!("{}/", h.path)));
        if app_wanted(&app, apps) && !covered {
            hits.push(Hit {
                app,
                path: path.to_string(),
            });
        }
    }
    hits
}

/// `--apps claude` matches `claude-code` and `claude-desktop`.
fn app_wanted(app: &str, apps: &[String]) -> bool {
    apps.is_empty()
        || apps
            .iter()
            .any(|a| app.starts_with(&a.to_ascii_lowercase()))
}

fn discover_remote(runner: &dyn Runner, m: &mut Machine, deep: bool, apps: &[String]) {
    let (prog, args) = remote_cmd(&m.kind);
    match runner.run(prog, &args, list_script(deep).as_bytes()) {
        Ok(o) if o.ok => m.hits = parse_hits(&o.stdout, apps),
        Ok(o) => {
            m.error = Some(format!("{prog} failed: {}", o.stderr));
            return;
        }
        Err(e) => {
            m.error = Some(format!("{e:#}"));
            return;
        }
    }
    let roots: Vec<_> = m
        .hits
        .iter()
        .flat_map(|h| discover::roots_for_hit(&h.path))
        .collect();
    if roots.is_empty() {
        return;
    }
    match runner.run(prog, &args, files_script(&roots).as_bytes()) {
        Ok(o) if o.ok => m.files = parse_files(&o.stdout, &roots),
        Ok(o) => m.error = Some(format!("{prog} failed listing files: {}", o.stderr)),
        Err(e) => m.error = Some(format!("{e:#}")),
    }
}

/// File names from a [`files_script`] run, re-checked against the shared store
/// rules and the discovered `roots`: anything the listing over-matched is dropped here. Names that cannot
/// go through a line-based list (newlines) are dropped too.
pub fn parse_files(out: &[u8], roots: &[(&'static discover::Store, String)]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    let skip = discover::local_skip_dirs();
    let mut files: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("F\t"))
        .filter(|p| p.starts_with('/') && !p.contains(['\0', '\r']))
        .filter(|p| {
            roots
                .iter()
                .any(|(_, r)| p.starts_with(&format!("{}/", r.trim_end_matches('/'))))
        })
        .filter(|p| !p.split('/').any(|c| skip.iter().any(|s| s == c)))
        .filter(|p| discover::path_wanted(Path::new(p), false))
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    files
}

// ---- local ------------------------------------------------------------------

/// Injectable view of the host so Windows logic is testable anywhere.
pub struct Host<'a> {
    pub os: LocalOs,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub is_dir: &'a dyn Fn(&Path) -> bool,
}

fn discover_local(host: &Host, m: &mut Machine, deep: bool, apps: &[String]) {
    let mut roots: Vec<LocalRoot> = discover::local_roots_with(host.os, host.env, host.is_dir);
    if deep {
        let home = match host.os {
            LocalOs::Windows => host.env("USERPROFILE"),
            LocalOs::Linux => host.env("HOME"),
        };
        if let Some(home) = home {
            roots.extend(deep_scan(Path::new(&home)));
        }
    }
    for r in roots {
        let path = r.path.to_string_lossy().into_owned();
        let covered = m
            .hits
            .iter()
            .any(|h| Path::new(&path).starts_with(Path::new(&h.path)));
        if app_wanted(r.app, apps) && !covered {
            m.hits.push(Hit {
                app: r.app.into(),
                path,
            });
        }
    }
}

impl Host<'_> {
    fn env(&self, k: &str) -> Option<String> {
        (self.env)(k).filter(|v| !v.is_empty())
    }
}

/// Bounded scan (depth 4 under home, never a whole disk) for known dot-dirs.
fn deep_scan(home: &Path) -> Vec<LocalRoot> {
    let names = discover::deep_dir_names();
    let skip = discover::local_skip_dirs();
    let mut out = Vec::new();
    let walk = walkdir::WalkDir::new(home).max_depth(4).into_iter();
    for e in walk.filter_entry(|e| {
        let n = e.file_name().to_string_lossy();
        !(e.file_type().is_dir() && (skip.iter().any(|s| s == &*n) || n == ".git" || n == ".cache"))
    }) {
        let Ok(e) = e else { continue };
        if !e.file_type().is_dir() {
            continue;
        }
        let n = e.file_name().to_string_lossy();
        if let Some((app, _)) = names.iter().find(|(_, d)| *d == &*n) {
            out.push(LocalRoot {
                app,
                path: e.path().to_path_buf(),
            });
        }
    }
    out
}

// ---- state ------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Key: `machine|original path`.
    pub roots: BTreeMap<String, RootState>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RootState {
    /// blake3 over sorted (relative path, size, mtime) of the store's files.
    pub fingerprint: String,
    pub files: u64,
    pub bytes: u64,
    pub done: bool,
}

impl State {
    /// A missing file is an empty state; an unreadable or malformed one is an
    /// error (never silently replaced, which would redo or lose progress).
    pub fn load(out: &Path) -> Result<State> {
        let path = out.join(STATE_FILE);
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| {
                format!(
                    "{} is malformed; fix or delete it to continue",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
    fn save(&self, out: &Path) -> Result<()> {
        write_atomic(&out.join(STATE_FILE), &serde_json::to_vec_pretty(self)?)
    }
}

fn fingerprint(root: &Path) -> (String, u64, u64) {
    let mut h = blake3::Hasher::new();
    let (mut files, mut bytes) = (0, 0);
    let mut entries: Vec<_> = walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| discover::path_wanted(e.path(), e.file_type().is_dir()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    for e in entries {
        let Ok(md) = e.metadata() else { continue };
        let mt = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let rel = e.path().strip_prefix(root).unwrap_or(e.path());
        h.update(format!("{}\0{}\0{mt}\n", rel.display(), md.len()).as_bytes());
        files += 1;
        bytes += md.len();
    }
    (h.finalize().to_hex().to_string(), files, bytes)
}

// ---- orchestration ----------------------------------------------------------

pub struct Request {
    pub local: bool,
    pub wsl: bool,
    pub ssh: Vec<String>,
    pub all_machines: bool,
    pub apps: Vec<String>,
    pub dry_run: bool,
    pub deep: bool,
    pub resume: bool,
    pub output: PathBuf,
    pub secrets: SecretPolicy,
    /// Contents of `~/.ssh/config`, for `--all-machines`.
    pub ssh_config: Option<String>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub machines: Vec<MachineResult>,
}

#[derive(Debug)]
pub struct MachineResult {
    pub id: String,
    pub label: String,
    pub apps: Vec<String>,
    pub collected_roots: usize,
    pub skipped_roots: usize,
    pub error: Option<String>,
}

impl Summary {
    /// Exit policy: non-zero only when every requested machine failed.
    pub fn exit_code(&self) -> i32 {
        let failed = self.machines.iter().filter(|m| m.error.is_some()).count();
        i32::from(!self.machines.is_empty() && failed == self.machines.len())
    }
}

fn app_label(app: &str) -> &str {
    match app {
        "claude-code" => "Claude Code",
        "claude-desktop" => "Claude",
        "chatgpt-desktop" => "ChatGPT",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "pi" => "Pi",
        "omp" => "OMP",
        "hermes" => "Hermes",
        "antigravity" => "Antigravity",
        "gemini" => "Gemini",
        "minimax" => "MiniMax",
        "deepseek" => "DeepSeek",
        "cursor" => "Cursor",
        other => other,
    }
}

fn app_list(hits: &[Hit]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for h in hits {
        let l = app_label(&h.app).to_string();
        if !v.contains(&l) {
            v.push(l);
        }
    }
    v
}

/// Resolve which machines were asked for and discover their stores (read-only;
/// nothing is copied). A machine that cannot be reached carries its error.
pub fn discover_machines(runner: &dyn Runner, host: &Host, req: &Request) -> Vec<Machine> {
    let mut machines = Vec::new();
    if req.local || req.all_machines {
        let mut m = machine(Kind::Local, host.os);
        discover_local(host, &mut m, req.deep, &req.apps);
        machines.push(m);
    }
    if req.wsl || req.all_machines {
        match runner.run("wsl.exe", &["-l".into(), "-q".into()], b"") {
            Ok(o) if o.ok => {
                for d in parse_wsl_list(&o.stdout) {
                    let mut m = machine(Kind::Wsl(d), host.os);
                    discover_remote(runner, &mut m, req.deep, &req.apps);
                    machines.push(m);
                }
            }
            // Not an error: most hosts have no WSL. Reported as a note.
            Ok(o) => eprintln!("note: `wsl.exe -l -q` failed ({}); skipping WSL", o.stderr),
            Err(_) => eprintln!("note: wsl.exe not found; skipping WSL"),
        }
    }
    let mut hosts = req.ssh.clone();
    if req.all_machines {
        for h in parse_ssh_hosts(req.ssh_config.as_deref().unwrap_or("")) {
            if !hosts.contains(&h) {
                hosts.push(h);
            }
        }
    }
    for h in hosts {
        let mut m = machine(Kind::Ssh(h.clone()), host.os);
        if h.starts_with('-') {
            m.error = Some("invalid host (looks like an option)".into());
        } else {
            discover_remote(runner, &mut m, req.deep, &req.apps);
        }
        machines.push(m);
    }
    machines
}

pub fn print_found(machines: &[Machine]) {
    for m in machines {
        match (&m.error, m.hits.is_empty()) {
            (Some(e), _) => println!("{}: unavailable ({e})", m.label),
            (None, true) => println!("{}: nothing found", m.label),
            (None, false) => println!("{}: {} found", m.label, app_list(&m.hits).join(", ")),
        }
    }
}

/// Stage a remote machine's hits under `<out>/collect-staging/<slug>`.
pub fn stage(runner: &dyn Runner, m: &Machine, out: &Path, resume: bool) -> Result<PathBuf> {
    let dir = out.join(STAGING_DIR).join(m.id.replace('/', "_"));
    if resume && (dir.join(COMPLETE_MARK).exists() || dir.join(LEGACY_COMPLETE_MARK).exists()) {
        return Ok(dir); // an earlier run fetched everything; staging kept until imported
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    if m.files.is_empty() {
        std::fs::write(dir.join(COMPLETE_MARK), b"")?;
        return Ok(dir); // nothing the parsers read: nothing is copied
    }
    let (prog, args) = remote_cmd(&m.kind);
    let o = runner.run(prog, &args, fetch_script(&m.files).as_bytes())?;
    // tar exits non-zero for a file that vanished mid-read; the rest is usable.
    if !o.ok && o.stdout.is_empty() {
        bail!("{prog} failed: {}", o.stderr);
    }
    let mut ar = tar::Archive::new(std::io::Cursor::new(&o.stdout));
    for e in ar.entries().context("reading tar stream")? {
        let mut e = e?;
        let t = e.header().entry_type();
        // Regular files and directories only: a remote must not plant links.
        if t.is_file() || t.is_dir() {
            e.unpack_in(&dir)?;
        }
    }
    std::fs::write(dir.join(COMPLETE_MARK), b"")?;
    Ok(dir)
}

fn collect_machine(
    runner: &dyn Runner,
    m: &Machine,
    req: &Request,
    state: &mut State,
) -> Result<(usize, usize)> {
    // (original path, readable path)
    let (roots, stage_dir): (Vec<(String, PathBuf)>, Option<PathBuf>) = match &m.kind {
        Kind::Local => (
            m.hits
                .iter()
                .map(|h| (h.path.clone(), PathBuf::from(&h.path)))
                .collect(),
            None,
        ),
        _ => {
            let dir = stage(runner, m, &req.output, req.resume)?;
            let roots = m
                .hits
                .iter()
                .map(|h| (h.path.clone(), dir.join(h.path.trim_start_matches('/'))))
                .filter(|(_, p)| p.exists())
                .collect();
            (roots, Some(dir))
        }
    };
    let mut todo: Vec<(String, PathBuf, String, u64, u64)> = Vec::new();
    let mut skipped = 0;
    for (orig, read) in roots {
        let (fp, files, bytes) = fingerprint(&read);
        let done = state
            .roots
            .get(&format!("{}|{orig}", m.id))
            .is_some_and(|s| s.done && s.fingerprint == fp);
        // Only --resume trusts saved progress; otherwise re-collect (dedup keeps it idempotent).
        if req.resume && done {
            skipped += 1;
        } else {
            todo.push((orig, read, fp, files, bytes));
        }
    }
    if !todo.is_empty() {
        for (orig, _, fp, files, bytes) in &todo {
            state.roots.insert(
                format!("{}|{orig}", m.id),
                RootState {
                    fingerprint: fp.clone(),
                    files: *files,
                    bytes: *bytes,
                    done: false,
                },
            );
        }
        state.save(&req.output)?;
        let opts = ImportOptions {
            output: req.output.clone(),
            resume: req.resume,
            secrets: req.secrets,
            machine_id: Some(m.id.clone()),
            platform: Some(
                match &m.kind {
                    Kind::Local if cfg!(windows) => "windows",
                    _ => "linux",
                }
                .into(),
            ),
            known_stores_only: true,
            path_map: stage_dir.as_ref().map(|d| (d.clone(), String::new())),
            args: std::env::args().skip(1).collect(),
            ..Default::default()
        };
        let mut imp = Importer::new(opts, Config::default(), crate::parsers::registry())?;
        let pb = crate::progress::create_import_progress(
            crate::progress::ProgressMode::Auto,
            Some(todo.len() as u64),
        );
        imp.set_progress(pb.clone());
        let read: Vec<PathBuf> = todo.iter().map(|t| t.1.clone()).collect();
        let imported = imp.import(&read)?;
        crate::progress::finish_progress(&pb, &format!("{}: done", m.label));
        for (orig, ..) in &todo {
            if let Some(s) = state.roots.get_mut(&format!("{}|{orig}", m.id)) {
                s.done = imported.sources_failed == 0;
            }
        }
        state.save(&req.output)?;
    }
    if let Some(d) = stage_dir {
        let _ = crate::scratch::remove_dir_all(&d); // imported: staging no longer needed
    }
    Ok((todo.len(), skipped))
}

/// Discover, then (unless `dry_run`) stage + import each machine. One failing
/// machine never aborts the others.
pub fn run(runner: &dyn Runner, host: &Host, req: &Request) -> Result<Summary> {
    if !(req.local || req.wsl || req.all_machines || !req.ssh.is_empty()) {
        bail!("name something to collect: --local, --wsl, --ssh USER@HOST or --all-machines");
    }
    let machines = discover_machines(runner, host, req);
    print_found(&machines);
    let mut summary = Summary::default();
    let mut state = if req.dry_run {
        State::default()
    } else {
        State::load(&req.output)?
    };
    if !req.dry_run {
        println!("\nCollecting...");
    }
    for m in &machines {
        let mut r = MachineResult {
            id: m.id.clone(),
            label: m.label.clone(),
            apps: app_list(&m.hits),
            collected_roots: 0,
            skipped_roots: 0,
            error: m.error.clone(),
        };
        if r.error.is_none() && !req.dry_run && !m.hits.is_empty() {
            match collect_machine(runner, m, req, &mut state) {
                Ok((c, s)) => (r.collected_roots, r.skipped_roots) = (c, s),
                Err(e) => r.error = Some(format!("{e:#}")),
            }
        }
        summary.machines.push(r);
    }
    Ok(summary)
}

/// Read once at the CLI edge.
pub fn read_ssh_config() -> Option<String> {
    std::fs::read_to_string(ssh_config_path(&|k| std::env::var(k).ok())?).ok()
}

/// `~/.ssh/config`, with `USERPROFILE` as the fallback when `HOME` is unset/empty.
pub fn ssh_config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.is_empty()))?;
    Some(Path::new(&home).join(".ssh").join("config"))
}
