//! Machine, project and repository identity.
//!
//! Paths are evidence, not identity. A project id derived from a bare path
//! string would merge unrelated projects that happen to share a name, and would
//! split one project across the several spellings a real backup contains
//! (`C:\src\foo`, `/mnt/c/src/foo`, `/home/me/foo`). So identity comes from, in
//! order of strength:
//!
//! 1. an explicit alias in the config;
//! 2. a Git remote recorded in `.git/config` (read as data, never executed);
//! 3. otherwise a path-derived key marked as low-confidence evidence.
//!
//! The original path spelling is always kept separately on the event.

use crate::config::Config;
use crate::id;
use std::path::{Path, PathBuf};

/// Normalize a path spelling for comparison: separators unified, WSL mounts
/// (`/mnt/c/x`) and Windows long-path prefixes (`\\?\C:\x`) collapsed to the
/// drive spelling. Only Windows-style paths (drive or UNC) are case-folded:
/// Unix paths are case-sensitive, and folding them would merge distinct
/// projects (`/home/u/Foo` and `/home/u/foo`).
pub fn normalize_path(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    // \\?\C:\x -> C:/x, \\?\UNC\srv\share -> //srv/share
    if let Some(rest) = p.strip_prefix("//?/").or_else(|| p.strip_prefix("//./")) {
        p = match rest.get(..4) {
            Some(u) if u.eq_ignore_ascii_case("unc/") => format!("//{}", &rest[4..]),
            _ => rest.to_string(),
        };
    }
    let unc = p.starts_with("//") && !p.starts_with("///");
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    if unc {
        p = format!("/{p}");
    }
    // WSL: /mnt/c/Users/x -> c:/Users/x.
    if let Some(rest) = p.strip_prefix("/mnt/") {
        let mut chars = rest.chars();
        let (d, sep) = (chars.next(), chars.next());
        if let Some(d) = d.filter(|d| d.is_ascii_alphabetic()) {
            if sep == Some('/') || (sep.is_none() && rest.len() == 1) {
                p = format!("{d}:/{}", chars.as_str());
            }
        }
    }
    let windows =
        unc || (p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic());
    let p = p.trim_end_matches('/').to_string();
    if windows {
        p.to_ascii_lowercase()
    } else {
        p
    }
}

/// True when `path` is `prefix` or lies below it (component boundary, never a
/// bare string prefix: `/srv/host-a` does not contain `/srv/host-a-old`).
pub fn path_within(path: &str, prefix: &str) -> bool {
    let (p, pre) = (normalize_path(path), normalize_path(prefix));
    p == pre || p.starts_with(&format!("{pre}/")) || pre.is_empty() && p.starts_with('/')
}

/// Last meaningful path component, used as a display name.
pub fn basename(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_end_matches('/');
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// Match a path against config machine rules. Returns `(machine_id, platform)`.
pub fn resolve_machine(cfg: &Config, path: &str) -> Option<(String, Option<String>)> {
    for rule in &cfg.machine {
        for prefix in &rule.paths {
            if path_within(path, prefix) {
                return Some((rule.id.clone(), rule.platform.clone()));
            }
        }
    }
    None
}

/// A platform hint that is *not* an identity: used only for reporting.
pub fn platform_hint(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    if p.starts_with("/mnt/c/") {
        return Some("windows (via wsl mount)".into());
    }
    if p.starts_with("/mnt/") {
        return Some("windows-volume (via wsl mount)".into());
    }
    if p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic() {
        return Some("windows".into());
    }
    if p.starts_with('/') {
        return Some("unix".into());
    }
    None
}

/// Read `remote.origin.url` (or any `[remote "..."]`) straight out of a
/// `.git/config`. The file is parsed as data; no git command is ever run.
pub fn git_remote(config_text: &str) -> Option<String> {
    let mut current_remote = false;
    for raw in config_text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let section = &line[1..line.len() - 1];
            current_remote = section.trim_start().starts_with("remote ");
            continue;
        }
        if current_remote {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    let v = v.trim().trim_matches('"').to_string();
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

/// Normalize a git remote URL into a provider-independent repository key.
pub fn normalize_remote(remote: &str) -> String {
    let mut r = remote.trim().to_string();
    if let Some(rest) = r.strip_prefix("git@") {
        // git@github.com:owner/repo.git -> github.com/owner/repo
        r = rest.replacen(':', "/", 1);
    } else if let Some(rest) = r.strip_prefix("ssh://") {
        r = rest
            .split_once('@')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| rest.to_string());
    } else if let Some((_, rest)) = r.split_once("://") {
        r = rest.to_string();
        if let Some((_, host_path)) = r.split_once('@') {
            r = host_path.to_string();
        }
    }
    let r = r.trim_end_matches('/').trim_end_matches(".git").to_string();
    r.to_ascii_lowercase()
}

/// Look for a `.git` directory (or worktree pointer file) at or above `start`,
/// bounded by `max_up` levels. Returns the repository root and remote if found.
/// Read a `.git/config` as data, bounded so a hostile tree cannot make the
/// importer slurp an arbitrary file.
pub fn read_git_config(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(1024 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

pub fn find_repo(start: &Path, max_up: usize) -> Option<(PathBuf, Option<String>)> {
    let mut cur = Some(start.to_path_buf());
    let mut level = 0;
    while let Some(dir) = cur {
        if level > max_up {
            break;
        }
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            let cfg = dotgit.join("config");
            let remote = read_git_config(&cfg).as_deref().and_then(git_remote);
            return Some((dir, remote));
        }
        if dotgit.is_file() {
            // Worktree: `.git` points at the real gitdir.
            if let Ok(text) = std::fs::read_to_string(&dotgit) {
                if let Some(rest) = text.trim().strip_prefix("gitdir:") {
                    let target = PathBuf::from(rest.trim());
                    let resolved = if target.is_absolute() {
                        target
                    } else {
                        dir.join(target)
                    };
                    // A linked worktree's gitdir (`.git/worktrees/x`) names the
                    // shared gitdir in `commondir`; config lives there.
                    let common_dir = read_git_config(&resolved.join("commondir"))
                        .map(|c| resolved.join(c.trim()))
                        .unwrap_or_else(|| resolved.clone());
                    let remote = read_git_config(&common_dir.join("config"))
                        .as_deref()
                        .and_then(git_remote);
                    return Some((dir, remote));
                }
            }
        }
        cur = dir.parent().map(|p| p.to_path_buf());
        level += 1;
    }
    None
}

#[derive(Debug, Clone)]
pub struct ResolvedProject {
    pub project_id: String,
    pub repository_id: Option<String>,
    pub name: String,
    pub evidence: Vec<String>,
}

/// Resolve a project identity from a working directory and optional repository
/// evidence, consulting the config aliases first.
pub fn resolve_project(
    cfg: &Config,
    working_directory: Option<&str>,
    remote: Option<&str>,
    local_root: Option<&Path>,
) -> Option<ResolvedProject> {
    let mut evidence = Vec::new();

    // 1. repository alias by remote
    let mut repository_id: Option<String> = None;
    let mut canonical_remote: Option<String> = None;
    if let Some(r) = remote {
        let norm = normalize_remote(r);
        for alias in &cfg.repository_alias {
            if alias.remotes.iter().any(|x| normalize_remote(x) == norm)
                || normalize_remote(&alias.repository) == norm
            {
                repository_id = Some(id::id("repo_", &["alias", &alias.repository]));
                canonical_remote = Some(alias.repository.clone());
                evidence.push(format!("repository alias match for {r}"));
                break;
            }
        }
        if repository_id.is_none() {
            repository_id = Some(id::id("repo_", &["remote", &norm]));
            canonical_remote = Some(norm.clone());
            evidence.push(format!("git remote {norm}"));
        }
    }

    // 2. project alias by path
    if let Some(wd) = working_directory {
        for alias in &cfg.project_alias {
            if alias.paths.iter().any(|p| path_within(wd, p)) {
                evidence.push(format!("project alias {:?} matched {wd}", alias.project));
                return Some(ResolvedProject {
                    project_id: id::id("prj_", &["alias", &alias.project]),
                    repository_id: repository_id.clone(),
                    name: alias.project.clone(),
                    evidence,
                });
            }
        }
    }

    // 3. repository identity alone is enough to name a project.
    if let Some(rep) = &repository_id {
        let name = canonical_remote
            .as_deref()
            .map(basename)
            .filter(|s| !s.is_empty())
            .or_else(|| working_directory.map(basename))
            .unwrap_or_else(|| "unknown".to_string());
        return Some(ResolvedProject {
            project_id: id::id("prj_", &["repo", rep]),
            repository_id: Some(rep.clone()),
            name,
            evidence,
        });
    }

    // 4. path-derived fallback, low confidence, still useful for grouping.
    let wd = working_directory?;
    let norm = normalize_path(wd);
    let name = basename(wd);
    if name.is_empty() {
        return None;
    }
    if let Some(root) = local_root {
        evidence.push(format!("no git evidence under {root:?}"));
    }
    evidence.push("path-derived project key (low confidence)".into());
    Some(ResolvedProject {
        project_id: id::id("prj_", &["path", &norm]),
        repository_id: None,
        name,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_across_machines() {
        assert_eq!(
            normalize_path("C:\\src\\foo"),
            normalize_path("/mnt/c/src/foo")
        );
        assert_eq!(normalize_path("C:\\SRC\\Foo\\"), "c:/src/foo");
        assert_eq!(normalize_path("/home/u/foo"), "/home/u/foo");
    }

    #[test]
    fn unix_paths_stay_case_sensitive_and_unc_is_distinct() {
        assert_ne!(normalize_path("/home/u/Foo"), normalize_path("/home/u/foo"));
        assert_eq!(normalize_path("\\\\Srv\\Share\\X"), "//srv/share/x");
        assert_eq!(normalize_path("//srv/share/x"), "//srv/share/x");
        assert_ne!(
            normalize_path("\\\\srv\\share\\x"),
            normalize_path("/srv/share/x")
        );
        assert_eq!(normalize_path("\\\\?\\C:\\x"), "c:/x");
        assert_eq!(normalize_path("\\\\?\\UNC\\srv\\share\\x"), "//srv/share/x");
        assert_eq!(normalize_path("/mnt/c"), "c:");
        assert_eq!(normalize_path("/mnt/cache/x"), "/mnt/cache/x");
    }

    #[test]
    fn path_within_respects_component_boundaries() {
        assert!(path_within("/srv/host-a/a", "/srv/host-a"));
        assert!(!path_within("/srv/host-a-old/a", "/srv/host-a"));
        assert!(path_within("/mnt/c/src/foo/x", "C:\\src\\foo"));
        assert!(path_within("C:\\Users\\a", "C:\\"));
        assert!(!path_within("/home/U/x", "/home/u"));
    }

    #[test]
    fn normalizes_remotes() {
        assert_eq!(
            normalize_remote("git@github.com:me/foo.git"),
            "github.com/me/foo"
        );
        assert_eq!(
            normalize_remote("https://github.com/me/foo.git"),
            "github.com/me/foo"
        );
        assert_eq!(
            normalize_remote("ssh://git@github.com/me/foo"),
            "github.com/me/foo"
        );
        assert_eq!(
            normalize_remote("https://user@github.com/me/foo/"),
            "github.com/me/foo"
        );
    }

    #[test]
    fn reads_remote_from_git_config() {
        let text = "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:me/foo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        assert_eq!(
            git_remote(text).as_deref(),
            Some("git@github.com:me/foo.git")
        );
        assert_eq!(git_remote("[core]\n\tbare = false\n"), None);
    }

    #[test]
    fn cross_machine_paths_share_repository_identity() {
        let cfg = Config::default();
        let a = resolve_project(
            &cfg,
            Some("C:\\src\\foo"),
            Some("git@github.com:me/foo.git"),
            None,
        )
        .unwrap();
        let b = resolve_project(
            &cfg,
            Some("/home/u/foo"),
            Some("https://github.com/me/foo.git"),
            None,
        )
        .unwrap();
        assert_eq!(a.repository_id, b.repository_id);
        assert_eq!(a.project_id, b.project_id);
    }

    #[test]
    fn path_only_projects_stay_distinct_across_machines() {
        let cfg = Config::default();
        let a = resolve_project(&cfg, Some("C:\\src\\foo"), None, None).unwrap();
        let b = resolve_project(&cfg, Some("/home/u/foo"), None, None).unwrap();
        assert_ne!(a.project_id, b.project_id, "no evidence means no merge");
        assert_eq!(a.name, b.name, "display names may still coincide");
    }

    #[test]
    fn detects_worktree_pointer() {
        let dir = std::env::temp_dir().join(format!("convolith-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let main = dir.join("main");
        let wt = dir.join("wt/foo-exp");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        std::fs::write(
            main.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/x.git\n",
        )
        .unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git").display()),
        )
        .unwrap();
        let (root, remote) = find_repo(&wt, 4).unwrap();
        assert_eq!(root, wt);
        assert_eq!(remote.as_deref(), Some("https://example.com/x.git"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
