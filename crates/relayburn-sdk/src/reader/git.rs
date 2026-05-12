//! Git config / remote-url helpers — Rust port of `packages/reader/src/git.ts`.
//!
//! Three pieces:
//!
//!   - [`parse_git_config`] reads `.git/config` text into a section map,
//!     handling `[remote "origin"]`-style subsections and stripping inline
//!     `#` / `;` comments outside quotes.
//!   - [`canonicalize_remote_url`] normalizes scp / https / ssh remote URLs
//!     into a stable `host/path` key, lowercasing the host but preserving
//!     owner/repo case.
//!   - [`ProjectResolver`] walks up from a cwd, opens `.git/config` (or
//!     follows a `.git` worktree pointer), and returns `{project, project_key}`
//!     with a per-instance cache. The free [`resolve_project`] uses a process
//!     global resolver for the common case; tests construct their own to
//!     avoid sharing global state.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub project: String,
    pub project_key: Option<String>,
}

/// Cached resolver. Construct one per scope (CLI, test, embedding) to avoid
/// sharing a process-global cache.
#[derive(Debug, Default)]
pub struct ProjectResolver {
    cache: Mutex<HashMap<String, ResolvedProject>>,
}

impl ProjectResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a project for `cwd`, consulting (and populating) the cache.
    /// Holds the cache lock across `resolve_uncached` so concurrent callers
    /// with the same `cwd` only do the filesystem walk once.
    pub fn resolve(&self, cwd: &str) -> ResolvedProject {
        let mut cache = self.cache.lock().unwrap();
        if let Some(hit) = cache.get(cwd) {
            return hit.clone();
        }
        let result = resolve_uncached(cwd);
        cache.insert(cwd.to_string(), result.clone());
        result
    }

    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

static GLOBAL_RESOLVER: LazyLock<ProjectResolver> = LazyLock::new(ProjectResolver::new);

/// Convenience wrapper around a process-global [`ProjectResolver`]. Embedders
/// that want their own cache (notably tests) should construct their own
/// `ProjectResolver` directly.
pub fn resolve_project(cwd: &str) -> ResolvedProject {
    GLOBAL_RESOLVER.resolve(cwd)
}

fn resolve_uncached(cwd: &str) -> ResolvedProject {
    let fallback = || ResolvedProject {
        project: cwd.to_string(),
        project_key: None,
    };
    let Some(git_dir) = find_git_dir(Path::new(cwd)) else {
        return fallback();
    };
    let Ok(text) = fs::read_to_string(git_dir.join("config")) else {
        return fallback();
    };
    let key = parse_git_config(&text)
        .get("remote \"origin\"")
        .and_then(|m| m.get("url"))
        .and_then(|url| canonicalize_remote_url(url));
    ResolvedProject {
        project: cwd.to_string(),
        project_key: key,
    }
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir: PathBuf = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for _ in 0..100 {
        let candidate = dir.join(".git");
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_dir() {
                return Some(candidate);
            }
            if meta.is_file() {
                if let Some(resolved) = resolve_worktree_git_dir(&candidate) {
                    return Some(resolved);
                }
            }
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
    None
}

fn resolve_worktree_git_dir(git_file: &Path) -> Option<PathBuf> {
    static GITDIR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^gitdir:\s*(.+?)\s*$").unwrap());
    let text = fs::read_to_string(git_file).ok()?;
    let raw = GITDIR_RE.captures(&text)?.get(1)?.as_str().to_string();
    let raw_path = Path::new(&raw);
    let gitdir = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        git_file.parent()?.join(raw_path)
    };
    if let Ok(text) = fs::read_to_string(gitdir.join("commondir")) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            let p = Path::new(trimmed);
            return Some(if p.is_absolute() {
                p.to_path_buf()
            } else {
                gitdir.join(p)
            });
        }
    }
    Some(gitdir)
}

/// Parse `.git/config` text into `{section -> {key -> value}}`. Handles
/// `[section]` and `[section "subsection"]` headers and ignores `#` / `;`
/// comments + blank lines. Inline comments outside quotes are stripped.
pub fn parse_git_config(text: &str) -> HashMap<String, HashMap<String, String>> {
    static SUBSECTION_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^([A-Za-z0-9._-]+)\s+"(.*)"$"#).unwrap());

    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for raw_line in text.split('\n') {
        let line = raw_line
            .trim_end_matches('\r')
            .trim_start_matches([' ', '\t'])
            .trim_end_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let raw = line[1..line.len() - 1].trim();
            let name = match SUBSECTION_RE.captures(raw) {
                Some(c) => format!("{} \"{}\"", &c[1], &c[2]),
                None => raw.to_string(),
            };
            out.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let Some(section) = current.as_ref() else {
            continue;
        };
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim().to_string();
        let value = strip_inline_comment(line[eq + 1..].trim());
        if key.is_empty() {
            continue;
        }
        out.get_mut(section).unwrap().insert(key, value);
    }
    out
}

fn strip_inline_comment(value: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    for ch in value.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes && (ch == '#' || ch == ';') {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// Canonicalize a remote URL into `host/path` form. Returns `None` for inputs
/// that aren't recognizable git URLs.
pub fn canonicalize_remote_url(url: &str) -> Option<String> {
    static SCP_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:[A-Za-z0-9_-]+)@([^:\s]+):(.+)$").unwrap());
    static SCHEME_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^([A-Za-z][A-Za-z0-9+.\-]*)://(.+)$").unwrap());

    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(c) = SCP_RE.captures(trimmed) {
        let host = c.get(1)?.as_str().to_lowercase();
        let path_part = strip_dot_git(
            c.get(2)?
                .as_str()
                .trim_start_matches('/')
                .trim_end_matches('/'),
        );
        if path_part.is_empty() {
            return None;
        }
        return Some(format!("{host}/{path_part}"));
    }

    if let Some(c) = SCHEME_RE.captures(trimmed) {
        let rest = c.get(2)?.as_str();
        let after_auth = match rest.find('@') {
            Some(idx) => &rest[idx + 1..],
            None => rest,
        };
        let slash = after_auth.find('/')?;
        let host = strip_port(&after_auth[..slash]).to_lowercase();
        if host.is_empty() {
            return None;
        }
        let path_part = strip_dot_git(after_auth[slash + 1..].trim_end_matches('/'));
        if path_part.is_empty() {
            return None;
        }
        return Some(format!("{host}/{path_part}"));
    }

    None
}

fn strip_dot_git(p: &str) -> String {
    p.strip_suffix(".git").unwrap_or(p).to_string()
}

fn strip_port(host: &str) -> &str {
    match host.find(':') {
        Some(idx) => &host[..idx],
        None => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn canonicalize_scp_form() {
        assert_eq!(
            canonicalize_remote_url("git@github.com:AgentWorkforce/burn.git").as_deref(),
            Some("github.com/AgentWorkforce/burn"),
        );
    }

    #[test]
    fn canonicalize_https_with_dot_git() {
        assert_eq!(
            canonicalize_remote_url("https://github.com/AgentWorkforce/burn.git").as_deref(),
            Some("github.com/AgentWorkforce/burn"),
        );
    }

    #[test]
    fn canonicalize_https_without_dot_git_with_subgroup() {
        assert_eq!(
            canonicalize_remote_url("https://gitlab.com/group/sub/repo").as_deref(),
            Some("gitlab.com/group/sub/repo"),
        );
    }

    #[test]
    fn canonicalize_https_with_user() {
        assert_eq!(
            canonicalize_remote_url("https://user:token@github.com/foo/bar.git").as_deref(),
            Some("github.com/foo/bar"),
        );
    }

    #[test]
    fn canonicalize_ssh_with_port() {
        assert_eq!(
            canonicalize_remote_url("ssh://git@github.com:22/AgentWorkforce/burn.git").as_deref(),
            Some("github.com/AgentWorkforce/burn"),
        );
    }

    #[test]
    fn canonicalize_lowercases_host_only() {
        assert_eq!(
            canonicalize_remote_url("git@GitHub.COM:AgentWorkforce/Burn.git").as_deref(),
            Some("github.com/AgentWorkforce/Burn"),
        );
    }

    #[test]
    fn canonicalize_returns_none_on_junk() {
        assert_eq!(canonicalize_remote_url(""), None);
        assert_eq!(canonicalize_remote_url("not a url"), None);
        assert_eq!(canonicalize_remote_url("https://example.com/"), None);
    }

    #[test]
    fn canonicalize_strips_trailing_slash() {
        assert_eq!(
            canonicalize_remote_url("https://github.com/foo/bar/").as_deref(),
            Some("github.com/foo/bar"),
        );
    }

    #[test]
    fn parse_simple_sections() {
        let cfg = parse_git_config(
            "\n[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:foo/bar.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        );
        assert_eq!(cfg["core"]["repositoryformatversion"], "0");
        assert_eq!(
            cfg["remote \"origin\""]["url"],
            "git@github.com:foo/bar.git"
        );
    }

    #[test]
    fn parse_ignores_comments_and_blanks() {
        let cfg = parse_git_config(
            "\n# a comment\n; another comment\n[remote \"origin\"]\n\turl = https://github.com/foo/bar ; inline comment\n",
        );
        assert_eq!(
            cfg["remote \"origin\""]["url"],
            "https://github.com/foo/bar"
        );
    }

    #[test]
    fn resolve_project_no_git() {
        let resolver = ProjectResolver::new();
        let dir = tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let got = resolver.resolve(&path);
        assert_eq!(got.project, path);
        assert_eq!(got.project_key, None);
    }

    #[test]
    fn resolve_project_with_git_dir() {
        let resolver = ProjectResolver::new();
        let root = tempdir().unwrap();
        let git_dir = root.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:foo/bar.git\n",
        )
        .unwrap();
        let nested = root.path().join("packages").join("a");
        fs::create_dir_all(&nested).unwrap();
        let got = resolver.resolve(&nested.to_string_lossy());
        assert_eq!(got.project_key.as_deref(), Some("github.com/foo/bar"));
    }

    #[test]
    fn resolve_project_worktree() {
        let resolver = ProjectResolver::new();
        let root = tempdir().unwrap();
        let common_git = root.path().join("main").join(".git");
        fs::create_dir_all(&common_git).unwrap();
        fs::write(
            common_git.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/foo/bar\n",
        )
        .unwrap();
        let worktree_dir = common_git.join("worktrees").join("branch-a");
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::write(worktree_dir.join("commondir"), "../..\n").unwrap();

        let worktree = root.path().join("worktree-a");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_dir.display()),
        )
        .unwrap();

        let got = resolver.resolve(&worktree.to_string_lossy());
        assert_eq!(got.project_key.as_deref(), Some("github.com/foo/bar"));
    }

    #[test]
    fn resolve_project_memoizes() {
        let resolver = ProjectResolver::new();
        let root = tempdir().unwrap();
        let git_dir = root.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:foo/bar.git\n",
        )
        .unwrap();
        let key = root.path().to_string_lossy().to_string();
        let a = resolver.resolve(&key);
        // Mutating the config should not change the cached result.
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:zzz/zzz.git\n",
        )
        .unwrap();
        let b = resolver.resolve(&key);
        assert_eq!(a.project_key, b.project_key);
    }
}
