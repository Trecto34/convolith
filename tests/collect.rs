//! `convolith collect` with a fake Runner (no real wsl/ssh) and a fake host.

use anyhow::{anyhow, Result};
use convolith::collect::{self, Host, Kind, Output, Request, Runner};
use convolith::discover::LocalOs;
use convolith::secrets::SecretPolicy;
use std::cell::RefCell;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("convolith-collect-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn utf16(s: &str) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFE];
    v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
    v
}

const CODEX_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/codex/simple.jsonl");
const STORE: &str = "/home/u/.codex/sessions";
const SESSION: &str = "home/u/.codex/sessions/2026/04/01/rollout-2026-04-01T10-00-00-x.jsonl";

/// tar stream holding one codex session, as the remote `tar -C /` would emit.
fn session_tar() -> Vec<u8> {
    let body = std::fs::read(CODEX_FIXTURE).unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_775_000_000);
    h.set_cksum();
    b.append_data(&mut h, SESSION, body.as_slice()).unwrap();
    b.into_inner().unwrap()
}

/// Answers by (program, first host-ish arg); records every call.
struct Fake {
    /// machine arg (distro / host) -> Some(ok) | None (cannot start)
    broken: Vec<&'static str>,
    wsl_list: Option<Vec<u8>>,
    calls: RefCell<Vec<String>>,
    /// Every script piped over stdin, in order.
    scripts: RefCell<Vec<String>>,
    /// What the remote file listing prints (default: the codex session plus decoys).
    listing: Option<String>,
}

impl Fake {
    fn new(broken: &[&'static str]) -> Fake {
        Fake {
            broken: broken.to_vec(),
            wsl_list: None,
            calls: RefCell::new(Vec::new()),
            scripts: RefCell::new(Vec::new()),
            listing: None,
        }
    }
}

impl Runner for Fake {
    fn run(&self, prog: &str, args: &[String], stdin: &[u8]) -> Result<Output> {
        self.calls
            .borrow_mut()
            .push(format!("{prog} {}", args.join(" ")));
        if prog == "wsl.exe" && args == ["-l", "-q"] {
            return match &self.wsl_list {
                Some(l) => Ok(Output {
                    ok: true,
                    stdout: l.clone(),
                    stderr: String::new(),
                }),
                None => Err(anyhow!("cannot start wsl.exe")),
            };
        }
        let pos = args.iter().position(|a| a == "-d" || a == "--").unwrap();
        let target = &args[pos + 1];
        if self.broken.iter().any(|b| target.ends_with(b)) {
            return Ok(Output {
                ok: false,
                stdout: vec![],
                stderr: "Connection refused".into(),
            });
        }
        let script = String::from_utf8_lossy(stdin);
        self.scripts.borrow_mut().push(script.to_string());
        let stdout = if script.contains("tar -cf") {
            session_tar()
        } else if script.contains("emit()") {
            // A listing that over-matches: the decoys must never be fetched.
            self.listing.clone().unwrap_or_else(|| {
                format!("F\t/{SESSION}\nF\t{STORE}/notes.txt\nF\t{STORE}/node_modules/x.jsonl\nF\t/etc/passwd\n")
            }).into_bytes()
        } else {
            format!("codex\t{STORE}\n").into_bytes()
        };
        Ok(Output {
            ok: true,
            stdout,
            stderr: String::new(),
        })
    }
}

fn host<'a>(
    env: &'a dyn Fn(&str) -> Option<String>,
    is_dir: &'a dyn Fn(&Path) -> bool,
) -> Host<'a> {
    Host {
        os: LocalOs::Linux,
        env,
        is_dir,
    }
}

fn req(out: &Path) -> Request {
    Request {
        local: false,
        wsl: false,
        ssh: vec![],
        all_machines: false,
        apps: vec![],
        dry_run: false,
        deep: false,
        resume: false,
        output: out.to_path_buf(),
        secrets: SecretPolicy::Redact,
        ssh_config: None,
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}
fn no_dir(_: &Path) -> bool {
    false
}

type Row = (String, Option<String>, Option<String>, Option<String>);

fn sources(out: &Path) -> Vec<Row> {
    let db = rusqlite::Connection::open(out.join("provenance/provenance.sqlite")).unwrap();
    let mut st = db
        .prepare("select original_path, machine_id, provider_guess, application_guess from source where status != 'unsupported'")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn windows_discovery_uses_known_locations_only() {
    let env = |k: &str| match k {
        "USERPROFILE" => Some("C:\\Users\\a".to_string()),
        "APPDATA" => Some("C:\\Users\\a\\AppData\\Roaming".to_string()),
        "LOCALAPPDATA" => Some("C:\\Users\\a\\AppData\\Local".to_string()),
        _ => None,
    };
    let known = [
        "C:\\Users\\a/.codex/sessions",
        "C:\\Users\\a/.hermes",
        "C:\\Users\\a\\AppData\\Roaming/Claude",
    ];
    let present = |p: &Path| known.iter().any(|k| p == Path::new(k));
    let h = Host {
        os: LocalOs::Windows,
        env: &env,
        is_dir: &present,
    };
    let mut r = req(Path::new("unused"));
    r.local = true;
    let ms = collect::discover_machines(&Fake::new(&[]), &h, &r);
    assert_eq!(ms.len(), 1);
    assert!(ms[0].id.starts_with("windows/"));
    let apps: Vec<&str> = ms[0].hits.iter().map(|h| h.app.as_str()).collect();
    assert_eq!(apps, ["codex", "hermes", "claude-desktop"]);

    r.apps = vec!["claude".into()];
    let ms = collect::discover_machines(&Fake::new(&[]), &h, &r);
    assert_eq!(ms[0].hits.len(), 1, "--apps filters by prefix");
}

#[test]
fn parses_utf16_wsl_list_and_ssh_hosts() {
    let l = collect::parse_wsl_list(&utf16(
        "example-distro\r\nexample-container\r\nUbuntu-22.04\r\n",
    ));
    assert_eq!(l, ["example-distro", "Ubuntu-22.04"]);
    assert_eq!(collect::parse_wsl_list(b"a\nb\n"), ["a", "b"]);
    let h = collect::parse_ssh_hosts(
        "Host host-a *.corp\n  HostName x\nHost !bad web ?x\nhost host-a\n",
    );
    assert_eq!(h, ["host-a", "web"]);
}

#[test]
fn wsl_failing_distro_does_not_abort_others_and_provenance_is_kept() {
    let out = tmp("wsl").join("arch");
    let mut f = Fake::new(&["broken"]);
    f.wsl_list = Some(utf16("example-distro\r\nbroken\r\n"));
    let mut r = req(&out);
    r.wsl = true;
    let s = collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    assert_eq!(s.machines.len(), 2);
    assert_eq!(s.machines[0].id, "wsl/example-distro");
    assert_eq!(s.machines[0].collected_roots, 1);
    assert!(s.machines[1]
        .error
        .as_deref()
        .unwrap()
        .contains("Connection refused"));
    assert_eq!(s.exit_code(), 0, "non-zero only when everything failed");

    let src = sources(&out);
    assert_eq!(src.len(), 1, "{src:?}");
    let (path, machine, provider, app) = &src[0];
    assert_eq!(
        path,
        &format!("/{SESSION}"),
        "original path, not the staging path"
    );
    assert_eq!(machine.as_deref(), Some("wsl/example-distro"));
    assert_eq!(provider.as_deref(), Some("openai"));
    assert_eq!(app.as_deref(), Some("codex"));
    assert!(
        !out.join("collect-staging/wsl_example-distro").exists(),
        "staging removed once imported"
    );
}

#[test]
fn missing_wsl_exe_is_skipped_not_fatal() {
    let mut r = req(Path::new("unused"));
    r.wsl = true;
    r.dry_run = true;
    let ms = collect::discover_machines(&Fake::new(&[]), &host(&no_env, &no_dir), &r);
    assert!(ms.is_empty());
}

#[test]
fn ssh_host_failure_is_isolated_and_all_failed_exits_nonzero() {
    let out = tmp("ssh").join("arch");
    let f = Fake::new(&["down.example"]);
    let mut r = req(&out);
    r.ssh = vec!["user@down.example".into(), "user@ssh-host.example".into()];
    let s = collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    assert!(s.machines[0].error.is_some());
    assert_eq!(s.machines[1].id, "ssh/ssh-host.example");
    assert_eq!(s.machines[1].collected_roots, 1);
    assert_eq!(s.exit_code(), 0);
    // ssh invoked through BatchMode with a connect timeout, host after `--`.
    assert!(f.calls.borrow()[0]
        .starts_with("ssh -o BatchMode=yes -o ConnectTimeout=10 -- user@down.example sh -s"));

    let out2 = tmp("ssh2").join("arch");
    let mut r = req(&out2);
    r.ssh = vec!["a@down.example".into()];
    let s = collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    assert_eq!(s.exit_code(), 1);

    r.ssh = vec!["-oProxyCommand=x".into()];
    let s = collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    assert!(s.machines[0]
        .error
        .as_deref()
        .unwrap()
        .contains("invalid host"));
}

#[test]
fn rerun_skips_unchanged_and_interrupted_state_resumes() {
    let out = tmp("resume").join("arch");
    let f = Fake::new(&[]);
    let mut r = req(&out);
    r.ssh = vec!["u@host-a".into()];
    let h = host(&no_env, &no_dir);
    let s = collect::run(&f, &h, &r).unwrap();
    assert_eq!(
        (s.machines[0].collected_roots, s.machines[0].skipped_roots),
        (1, 0)
    );
    let s = collect::run(&f, &h, &r).unwrap();
    assert_eq!(
        (s.machines[0].collected_roots, s.machines[0].skipped_roots),
        (1, 0),
        "without --resume saved progress is not trusted"
    );
    r.resume = true;
    let s = collect::run(&f, &h, &r).unwrap();
    assert_eq!(
        (s.machines[0].collected_roots, s.machines[0].skipped_roots),
        (0, 1)
    );

    // Simulate a kill after the state was marked in-progress.
    let mut st = collect::State::load(&out).unwrap();
    assert!(st.roots.values().all(|r| r.done));
    for v in st.roots.values_mut() {
        v.done = false;
    }
    std::fs::write(
        out.join(collect::STATE_FILE),
        serde_json::to_vec(&st).unwrap(),
    )
    .unwrap();
    r.resume = true;
    let s = collect::run(&f, &h, &r).unwrap();
    assert_eq!(s.machines[0].collected_roots, 1);
    assert!(collect::State::load(&out)
        .unwrap()
        .roots
        .values()
        .all(|r| r.done));
}

#[test]
fn resume_reuses_complete_staging_without_the_network() {
    let out = tmp("stage");
    let mut m = collect::machine(Kind::Ssh("u@h".into()), LocalOs::Linux);
    m.hits.push(collect::Hit {
        app: "codex".into(),
        path: STORE.into(),
    });
    m.files.push(format!("/{SESSION}"));
    let dir = collect::stage(&Fake::new(&[]), &m, &out, false).unwrap();
    assert!(dir.join(SESSION).is_file());
    let f = Fake::new(&[]);
    collect::stage(&f, &m, &out, true).unwrap();
    assert!(
        f.calls.borrow().is_empty(),
        "complete staging is reused on --resume"
    );
}

#[test]
fn resume_honours_the_legacy_complete_marker() {
    let out = tmp("stage-legacy");
    let mut m = collect::machine(Kind::Ssh("u@h".into()), LocalOs::Linux);
    m.hits.push(collect::Hit {
        app: "codex".into(),
        path: STORE.into(),
    });
    m.files.push(format!("/{SESSION}"));
    let dir = collect::stage(&Fake::new(&[]), &m, &out, false).unwrap();
    std::fs::rename(
        dir.join(".convolith-complete"),
        dir.join(".aichive-complete"),
    )
    .unwrap();
    let f = Fake::new(&[]);
    collect::stage(&f, &m, &out, true).unwrap();
    assert!(
        f.calls.borrow().is_empty(),
        "legacy marker is still honoured"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let out = tmp("dry").join("arch");
    let f = Fake::new(&[]);
    let mut r = req(&out);
    r.ssh = vec!["u@host-a".into()];
    r.dry_run = true;
    let s = collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    assert_eq!(s.machines[0].apps, ["Codex"]);
    assert!(!out.exists());
    assert_eq!(
        f.calls.borrow().len(),
        2,
        "directory + file listing, no fetch"
    );
    assert!(!f.scripts.borrow().iter().any(|s| s.contains("tar -cf")));
}

#[test]
fn local_store_is_imported_in_place_with_machine_id() {
    let home = tmp("home");
    let d = home.join(".codex/sessions/2026");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::copy(CODEX_FIXTURE, d.join("rollout-2026-04-01T10-00-00-y.jsonl")).unwrap();
    let out = tmp("local").join("arch");
    let hs = home.to_string_lossy().into_owned();
    let hs2 = hs.clone();
    let env = move |k: &str| (k == "HOME").then(|| hs2.clone());
    let mut r = req(&out);
    r.local = true;
    let s = collect::run(&Fake::new(&[]), &host(&env, &|p| p.is_dir()), &r).unwrap();
    assert_eq!(s.machines[0].collected_roots, 1);
    let src = sources(&out);
    assert!(src[0].1.as_deref().unwrap().starts_with("linux/"));
    assert!(src[0]
        .0
        .starts_with(&home.canonicalize().unwrap().to_string_lossy().to_string()));
}

#[test]
fn malformed_state_is_an_error_and_left_untouched() {
    let out = tmp("badstate").join("arch");
    std::fs::create_dir_all(&out).unwrap();
    let sf = out.join(collect::STATE_FILE);
    std::fs::write(&sf, b"{ not json").unwrap();
    let mut r = req(&out);
    r.ssh = vec!["u@host-a".into()];
    let err = collect::run(&Fake::new(&[]), &host(&no_env, &no_dir), &r).unwrap_err();
    assert!(format!("{err:#}").contains("malformed"), "{err:#}");
    assert_eq!(std::fs::read(&sf).unwrap(), b"{ not json");
    assert!(collect::State::load(&tmp("nostate"))
        .unwrap()
        .roots
        .is_empty());
}

#[test]
fn ssh_config_path_falls_back_to_userprofile() {
    let env = |k: &str| match k {
        "HOME" => Some(String::new()),
        "USERPROFILE" => Some("C:\\Users\\a".to_string()),
        _ => None,
    };
    let p = collect::ssh_config_path(&env).unwrap();
    assert!(
        p.starts_with("C:\\Users\\a") && p.ends_with("config"),
        "{p:?}"
    );
    assert!(collect::ssh_config_path(&no_env).is_none());
}

#[cfg(windows)]
#[test]
fn locked_windows_source_is_failed_and_retried_after_unlock() {
    use std::os::windows::fs::OpenOptionsExt;
    let home = tmp("locked-home");
    let store = home.join(".codex/sessions");
    std::fs::create_dir_all(&store).unwrap();
    let locked = store.join("locked.jsonl");
    std::fs::copy(CODEX_FIXTURE, &locked).unwrap();
    std::fs::copy(CODEX_FIXTURE, store.join("readable.jsonl")).unwrap();
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();
    let env = |k: &str| (k == "USERPROFILE").then(|| home.to_string_lossy().into_owned());
    let host = Host {
        os: LocalOs::Windows,
        env: &env,
        is_dir: &|p| p.is_dir(),
    };
    let out = tmp("locked-out");
    let mut r = req(&out);
    r.local = true;
    collect::run(&Fake::new(&[]), &host, &r).unwrap();
    let stats = convolith::report::stats(&out).unwrap();
    assert_eq!(stats.sources_failed, 1);
    assert_eq!(stats.events_indexed, 4);
    assert!(collect::State::load(&out)
        .unwrap()
        .roots
        .values()
        .all(|r| !r.done));
    drop(handle);
    r.resume = true;
    let result = collect::run(&Fake::new(&[]), &host, &r).unwrap();
    assert_eq!(result.machines[0].collected_roots, 1);
    let stats = convolith::report::stats(&out).unwrap();
    assert_eq!(stats.sources_failed, 0);
    assert_eq!(stats.events_indexed, 4);
    assert_eq!(stats.observations, 8);
    assert!(collect::State::load(&out)
        .unwrap()
        .roots
        .values()
        .all(|r| r.done));
}

#[test]
fn local_machine_keys_are_stable_private_and_distinct() {
    let a = collect::local_machine_id(LocalOs::Windows, "01234567-89AB-CDEF-0123-456789ABCDEF\r\n");
    assert_eq!(
        a,
        collect::local_machine_id(LocalOs::Windows, "01234567-89ab-cdef-0123-456789abcdef")
    );
    assert_ne!(
        a,
        collect::local_machine_id(LocalOs::Windows, "fedcba98-7654-3210-fedc-ba9876543210")
    );
    assert_ne!(
        a,
        collect::local_machine_id(LocalOs::Linux, "01234567-89ab-cdef-0123-456789abcdef")
    );
    assert_eq!(a.len(), "windows/".len() + 24);
    assert_eq!(a, "windows/0a27e9185e809ee624d8ba94");
    assert_eq!(
        collect::local_machine_id(LocalOs::Linux, "01234567-89ab-cdef-0123-456789abcdef"),
        "linux/bab77d330b1452aebe282e20"
    );
    assert!(!a.contains("01234567"));
    assert_eq!(
        collect::machine(Kind::Wsl("example-distro".into()), LocalOs::Windows).id,
        "wsl/example-distro"
    );
    assert_eq!(
        collect::machine(Kind::Ssh("user@ssh-host.example".into()), LocalOs::Windows).id,
        "ssh/ssh-host.example"
    );
}

#[test]
fn generic_machine_archive_remains_idempotent_with_stable_local_identity() {
    use convolith::importer::{ImportOptions, Importer};
    let home = tmp("legacy-home");
    let store = home.join(".codex/sessions");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::copy(CODEX_FIXTURE, store.join("rollout.jsonl")).unwrap();
    let out = tmp("legacy-out");
    let opts = ImportOptions {
        output: out.clone(),
        machine_id: Some("linux".into()),
        known_stores_only: true,
        ..Default::default()
    };
    let mut importer = Importer::new(
        opts,
        convolith::config::Config::default(),
        convolith::parsers::registry(),
    )
    .unwrap();
    importer.import(std::slice::from_ref(&store)).unwrap();
    drop(importer);
    let ids = |out: &Path| {
        let db = rusqlite::Connection::open(out.join("provenance/provenance.sqlite")).unwrap();
        let mut stmt = db
            .prepare("select event_id,machine_id from event_index order by event_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
    };
    let before = ids(&out);
    let legacy = serde_json::json!({"roots": {format!("linux|{}", store.display()): {"fingerprint":"legacy", "files":1, "bytes":1800, "done":true}}});
    std::fs::write(
        out.join(collect::STATE_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let env = |k: &str| (k == "HOME").then(|| home.to_string_lossy().into_owned());
    let h = host(&env, &|p| p.is_dir());
    let mut r = req(&out);
    r.local = true;
    r.resume = true;
    assert_eq!(
        collect::run(&Fake::new(&[]), &h, &r).unwrap().machines[0].collected_roots,
        1
    );
    assert_eq!(sources(&out)[0].1.as_deref(), Some("linux"));
    assert_eq!(
        ids(&out),
        before,
        "existing event ids and machine fields remain intact"
    );
    assert_eq!(
        collect::run(&Fake::new(&[]), &h, &r).unwrap().machines[0].skipped_roots,
        1
    );
    r.resume = false;
    collect::run(&Fake::new(&[]), &h, &r).unwrap();
    assert_eq!(ids(&out), before);
    assert_eq!(convolith::report::stats(&out).unwrap().observations, 4);
    std::fs::copy(CODEX_FIXTURE, store.join("new-observation.jsonl")).unwrap();
    r.resume = true;
    collect::run(&Fake::new(&[]), &h, &r).unwrap();
    assert!(sources(&out)
        .iter()
        .any(|s| s.1.as_ref().unwrap().starts_with("linux/")));
    assert_eq!(ids(&out), before);
    assert_eq!(convolith::report::stats(&out).unwrap().observations, 8);
}

// ---- targeted remote collection ---------------------------------------------

#[test]
fn remote_fetch_lists_files_not_directories_and_drops_decoys() {
    let out = tmp("targeted").join("arch");
    let mut f = Fake::new(&[]);
    f.wsl_list = Some(utf16("example-distro\r\n"));
    let mut r = req(&out);
    r.wsl = true;
    collect::run(&f, &host(&no_env, &no_dir), &r).unwrap();
    let scripts = f.scripts.borrow();
    assert_eq!(
        scripts.len(),
        3,
        "dirs, then files, then one tar: {scripts:?}"
    );
    let fetch = &scripts[2];
    assert!(fetch.contains("tar -cf - -T -"), "{fetch}");
    assert!(fetch.contains(SESSION));
    for decoy in ["notes.txt", "node_modules", "etc/passwd"] {
        assert!(!fetch.contains(decoy), "{decoy} must not be transferred");
    }
    // The only names in the list are files; the store directory itself is not.
    let list: Vec<&str> = fetch
        .lines()
        .skip(1)
        .take_while(|l| *l != "CONVOLITH_FILE_LIST")
        .collect();
    assert_eq!(list, vec![SESSION]);
    // The file listing was generated from the shared rules.
    assert!(
        scripts[1].contains("find '/home/u/.codex/sessions'"),
        "{}",
        scripts[1]
    );
    assert!(scripts[1].contains("-name '*.jsonl'"));
}

#[test]
fn remote_selection_applies_every_store_rule_with_companions_and_siblings() {
    let listing = [
        // gemini: chats + the sibling .project_root, not logs/other files
        "/home/u/.gemini/tmp/p1/chats/session-1.jsonl",
        "/home/u/.gemini/tmp/p1/.project_root",
        "/home/u/.gemini/tmp/p1/logs.json",
        "/home/u/.gemini/tmp/p1/shell_history",
        // antigravity: transcript_full only
        "/home/u/.gemini/antigravity-cli/brain/b1/.system_generated/logs/transcript_full.jsonl",
        "/home/u/.gemini/antigravity-cli/brain/b1/.system_generated/logs/transcript.jsonl",
        "/home/u/.gemini/antigravity-cli/brain/b1/work/big.bin",
        "/home/u/.gemini/antigravity-cli/conversations/c.db",
        // minimax: live + snapshots; not metadata/sqlite/tool outputs
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/messages.jsonl",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/snapshots/g000000000001--ctx_a.jsonl",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/snapshots/env-g000000000001--ctx_a.json",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/manifest.json",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/reports/tool-outputs/o.txt",
        "/home/u/.minimax/v2/sqlite/runtime-state.sqlite",
        "/home/u/.minimax/.builtin-skills/pdf/a.json",
        // deepseek dsh
        "/home/u/.dsh/sessions/ws/session-1/session.v4.jsonl.zstd",
        "/home/u/.dsh/storages/workspace.json",
        // hermes: db + companions; not kanban/other dbs or the source checkout
        "/home/u/.hermes/state.db",
        "/home/u/.hermes/state.db-wal",
        "/home/u/.hermes/state.db-shm",
        "/home/u/.hermes/state.db-journal",
        "/home/u/.hermes/kanban.db",
        "/home/u/.hermes/hermes-agent/package.json",
        "/home/u/.hermes/profiles/hacker/state.db",
        "/home/u/.hermes/profiles/hacker/state.db-wal",
        "/home/u/.hermes/sessions/session_1.json",
        "/home/u/.hermes/sessions/request_dump_1.json",
        // opencode
        "/home/u/.local/share/opencode/opencode.db",
        "/home/u/.local/share/opencode/opencode.db-wal",
        "/home/u/.local/share/opencode/log/x.log",
        "/home/u/.local/share/opencode/auth.json",
        "/home/u/.local/share/opencode/storage/session/p/ses_1.json",
    ];
    let out: String = listing.iter().map(|p| format!("F\t{p}\n")).collect();
    let roots: Vec<_> = [
        "/home/u/.gemini",
        "/home/u/.minimax",
        "/home/u/.dsh",
        "/home/u/.hermes",
        "/home/u/.local/share/opencode",
    ]
    .iter()
    .flat_map(|p| convolith::discover::roots_for_hit(p))
    .collect();
    let got = collect::parse_files(out.as_bytes(), &roots);
    let want = [
        "/home/u/.dsh/sessions/ws/session-1/session.v4.jsonl.zstd",
        "/home/u/.gemini/antigravity-cli/brain/b1/.system_generated/logs/transcript_full.jsonl",
        "/home/u/.gemini/tmp/p1/.project_root",
        "/home/u/.gemini/tmp/p1/chats/session-1.jsonl",
        "/home/u/.hermes/profiles/hacker/state.db",
        "/home/u/.hermes/profiles/hacker/state.db-wal",
        "/home/u/.hermes/sessions/session_1.json",
        "/home/u/.hermes/state.db",
        "/home/u/.hermes/state.db-journal",
        "/home/u/.hermes/state.db-shm",
        "/home/u/.hermes/state.db-wal",
        "/home/u/.local/share/opencode/opencode.db",
        "/home/u/.local/share/opencode/opencode.db-wal",
        "/home/u/.local/share/opencode/storage/session/p/ses_1.json",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/messages.jsonl",
        "/home/u/.minimax/v2/sessions/2026/10/02/t-session_x/snapshots/g000000000001--ctx_a.jsonl",
    ];
    assert_eq!(got, want);
}

#[test]
fn deep_hit_expands_to_nested_store_roots() {
    let roots = convolith::discover::roots_for_hit("/home/u/proj/.gemini");
    let mut got: Vec<String> = roots.iter().map(|(_, p)| p.clone()).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "/home/u/proj/.gemini/antigravity",
            "/home/u/proj/.gemini/antigravity-cli",
            "/home/u/proj/.gemini/tmp"
        ]
    );
    let direct = convolith::discover::roots_for_hit("/home/u/.minimax");
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].1, "/home/u/.minimax");
}

/// Runs the generated scripts with the real `sh` against a fake HOME: the shell
/// logic itself must select only the intended files.
#[cfg(unix)]
#[test]
fn generated_scripts_select_only_intended_files_in_a_fake_home() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let home = tmp("fakehome");
    let put = |rel: &str, body: &str| {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    let wanted = [
        ".minimax/v2/sessions/2026/10/02/t-session_x/messages.jsonl",
        ".minimax/v2/sessions/2026/10/02/t-session_x/snapshots/g000000000001--ctx_a.jsonl",
        ".gemini/tmp/p1/chats/session-1.jsonl",
        ".gemini/tmp/p1/.project_root",
        ".dsh/sessions/ws/session-1/session.v4.jsonl.zstd",
        ".hermes/state.db",
        ".hermes/state.db-wal",
        ".hermes/state.db-shm",
        ".codex/sessions/2026/04/01/rollout-a.jsonl",
    ];
    let decoys = [
        ".minimax/v2/sessions/2026/10/02/t-session_x/manifest.json",
        ".minimax/v2/sessions/2026/10/02/t-session_x/reports/tool-outputs/o.txt",
        ".minimax/v2/sqlite/runtime-state.sqlite",
        ".minimax/.builtin-skills/pdf/SKILL.md",
        ".gemini/tmp/p1/logs.json",
        ".gemini/antigravity-cli/brain/b1/work/big.bin",
        ".dsh/storages/workspace.json",
        ".hermes/kanban.db",
        ".hermes/hermes-agent/package.json",
        ".codex/sessions/2026/04/01/notes.txt",
        ".codex/sessions/node_modules/dep.jsonl",
    ];
    for f in wanted.iter().chain(&decoys) {
        put(f, "x");
    }
    let sh = |script: &str| -> String {
        let mut c = Command::new("sh")
            .arg("-s")
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .current_dir(&home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap()
    };
    let hits = sh(&collect::list_script(false));
    let roots: Vec<_> = hits
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .flat_map(|(_, p)| convolith::discover::roots_for_hit(p))
        .collect();
    let files = collect::parse_files(sh(&collect::files_script(&roots)).as_bytes(), &roots);
    let mut want: Vec<String> = wanted
        .iter()
        .map(|w| format!("{}/{w}", home.display()))
        .collect();
    want.sort();
    assert_eq!(
        files, want,
        "shell listing + shared filter select exactly the wanted files"
    );

    // The fetch script archives exactly that list (explicit names, no directories).
    let tar_bytes = {
        let mut c = Command::new("sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin
            .take()
            .unwrap()
            .write_all(collect::fetch_script(&files).as_bytes())
            .unwrap();
        c.wait_with_output().unwrap().stdout
    };
    let mut ar = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut names: Vec<String> = ar
        .entries()
        .unwrap()
        .map(|e| format!("/{}", e.unwrap().path().unwrap().display()))
        .collect();
    names.sort();
    assert_eq!(names, files);
}

/// Manual check: run the generated remote scripts against the real `$HOME` and
/// print counts only (`cargo test --test collect real_home -- --ignored --nocapture`).
#[cfg(unix)]
#[test]
#[ignore]
fn real_home_script_dry_run_counts() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let sh = |script: &[u8]| -> Vec<u8> {
        let mut c = Command::new("sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(script).unwrap();
        c.wait_with_output().unwrap().stdout
    };
    let hits = String::from_utf8(sh(collect::list_script(false).as_bytes())).unwrap();
    let roots: Vec<_> = hits
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .flat_map(|(_, p)| convolith::discover::roots_for_hit(p))
        .collect();
    let files = collect::parse_files(&sh(collect::files_script(&roots).as_bytes()), &roots);
    let bytes: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    let tar = sh(collect::fetch_script(&files).as_bytes());
    println!(
        "store dirs: {}, roots: {}, files selected: {}, bytes: {}, tar stream bytes: {}",
        hits.lines().count(),
        roots.len(),
        files.len(),
        bytes,
        tar.len()
    );
    let mut by_root: Vec<(String, usize)> = roots
        .iter()
        .map(|(_, r)| {
            (
                r.clone(),
                files
                    .iter()
                    .filter(|f| f.starts_with(&format!("{r}/")))
                    .count(),
            )
        })
        .collect();
    by_root.sort();
    for (r, n) in by_root {
        println!("  {n:>5} files under {r}");
    }
}
