//! Harness-accurate startup instruction-file discovery for overhead
//! attribution: which `CLAUDE.md` / `AGENTS.md` files each harness injects
//! into its cached prompt prefix, in prompt order.

use std::fs;
use std::path::{Path, PathBuf};

use super::{OverheadFile, OverheadFileKind, OverheadFileScope};
use crate::reader::SourceKind;

pub(super) const DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct DiscoveryRoots {
    home: PathBuf,
    codex_home: PathBuf,
    opencode_config: PathBuf,
    /// Test-only filesystem-root substitute. Production always uses `None`
    /// and walks Claude ancestors to the real root.
    claude_ancestor_stop: Option<PathBuf>,
}

impl DiscoveryRoots {
    fn from_process() -> Self {
        let home = crate::util::home_dir();
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let opencode_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode");
        Self {
            home,
            codex_home,
            opencode_config,
            claude_ancestor_stop: None,
        }
    }

    fn for_harness_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            codex_home: home.join(".codex"),
            opencode_config: home.join(".config").join("opencode"),
            claude_ancestor_stop: None,
        }
    }

    #[cfg(test)]
    pub(super) fn for_home(home: &Path) -> Self {
        Self {
            claude_ancestor_stop: Some(home.to_path_buf()),
            ..Self::for_harness_home(home)
        }
    }
}

#[derive(Debug)]
struct DiscoveredFile {
    identity: FileIdentity,
    file: OverheadFile,
}

#[derive(Debug, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix {
        device: u64,
        inode: u64,
    },
    Canonical(PathBuf),
}

fn file_identity(path: &Path) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if let Ok(metadata) = fs::metadata(path) {
            return FileIdentity::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
        }
    }
    FileIdentity::Canonical(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn is_file(path: &Path) -> bool {
    matches!(fs::metadata(path), Ok(meta) if meta.is_file())
}

fn readable_nonempty_bytes(path: &Path) -> Option<Vec<u8>> {
    if !is_file(path) {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    (!String::from_utf8_lossy(&bytes).trim().is_empty()).then_some(bytes)
}

fn add_file(
    out: &mut Vec<DiscoveredFile>,
    kind: OverheadFileKind,
    path: &Path,
    scope: OverheadFileScope,
    source: SourceKind,
    content_bytes: usize,
) {
    let Some(bytes) = readable_nonempty_bytes(path) else {
        return;
    };
    let content_bytes = content_bytes.min(bytes.len());
    if content_bytes == 0
        || String::from_utf8_lossy(&bytes[..content_bytes])
            .trim()
            .is_empty()
    {
        return;
    }

    // Canonical identity collapses symlink aliases such as
    // `.claude/CLAUDE.md -> ../CLAUDE.md`. Keep different filename kinds as
    // separate rows: a root `CLAUDE.md -> AGENTS.md` represents disjoint
    // harness conventions even though the bytes happen to share a target.
    let identity = file_identity(path);
    if let Some(existing) = out.iter_mut().find(|entry| {
        entry.identity == identity
            && entry.file.kind == kind
            && entry.file.content_bytes == content_bytes
    }) {
        if !existing.file.applies_to.contains(&source) {
            existing.file.applies_to.push(source);
        }
        return;
    }
    if let Some(existing) = out.iter_mut().find(|entry| {
        entry.identity == identity
            && entry.file.kind == kind
            && entry.file.applies_to.contains(&source)
    }) {
        // The same harness can encounter a physical file through aliases.
        // Charge it once, using the shortest prefix that harness injects.
        existing.file.content_bytes = existing.file.content_bytes.min(content_bytes);
        return;
    }

    out.push(DiscoveredFile {
        identity,
        file: OverheadFile {
            kind,
            path: path.to_string_lossy().into_owned(),
            scope,
            applies_to: vec![source],
            content_bytes,
        },
    });
}

fn nearest_git_root(project_path: &Path) -> Option<PathBuf> {
    project_path
        .ancestors()
        .find(|dir| is_file(&dir.join(".git")) || dir.join(".git").is_dir())
        .map(Path::to_path_buf)
}

fn bounded_project_chain(project_path: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = git_root else {
        return vec![project_path.to_path_buf()];
    };
    let mut chain = Vec::new();
    for dir in project_path.ancestors() {
        chain.push(dir.to_path_buf());
        if dir == root {
            break;
        }
    }
    chain.reverse();
    chain
}

pub(crate) fn find_overhead_files(project_path: &Path) -> Vec<OverheadFile> {
    find_overhead_files_with_roots(project_path, &DiscoveryRoots::from_process())
}

pub(crate) fn find_overhead_files_in_home(project_path: &Path, home: &Path) -> Vec<OverheadFile> {
    find_overhead_files_with_roots(project_path, &DiscoveryRoots::for_harness_home(home))
}

/// Discover only instruction files that the default harness configuration
/// injects at session startup.
///
/// Deliberately excluded: on-demand descendant instructions (Claude Code and
/// OpenCode), Claude managed policy / `.claude/rules` / imports / excludes,
/// Codex fallback filenames and non-default byte caps, and OpenCode
/// `CONTEXT.md`, custom `instructions`, and compatibility-disable flags.
/// These require session or harness configuration evidence that this pure
/// filesystem query does not have; undercounting them is safer than charging
/// every project turn for a merely-present file.
pub(super) fn find_overhead_files_with_roots(
    project_path: &Path,
    roots: &DiscoveryRoots,
) -> Vec<OverheadFile> {
    let git_root = nearest_git_root(project_path);
    let project_scope_root = git_root.as_deref().unwrap_or(project_path);
    let project_chain = bounded_project_chain(project_path, git_root.as_deref());
    let mut found = Vec::<DiscoveredFile>::new();

    // User-global files are ordered before project files, matching all three
    // harnesses' prompt construction.
    add_file(
        &mut found,
        OverheadFileKind::ClaudeMd,
        &roots.home.join(".claude").join("CLAUDE.md"),
        OverheadFileScope::User,
        SourceKind::ClaudeCode,
        usize::MAX,
    );
    add_codex_user_file(&mut found, roots);
    add_opencode_user_file(&mut found, roots);
    add_claude_ancestor_chain(&mut found, project_path, project_scope_root, roots);
    add_codex_project_chain(&mut found, &project_chain);
    add_opencode_project_chain(&mut found, &project_chain);

    found.into_iter().map(|entry| entry.file).collect()
}

/// Codex global precedence is first non-empty: override, then AGENTS.md.
fn add_codex_user_file(found: &mut Vec<DiscoveredFile>, roots: &DiscoveryRoots) {
    let chosen = ["AGENTS.override.md", "AGENTS.md"]
        .into_iter()
        .map(|name| roots.codex_home.join(name))
        .find_map(|path| readable_nonempty_bytes(&path).map(|bytes| (path, bytes.len())));
    if let Some((path, len)) = chosen {
        add_file(
            found,
            OverheadFileKind::AgentsMd,
            &path,
            OverheadFileScope::User,
            SourceKind::Codex,
            len,
        );
    }
}

/// OpenCode stops at the first existing global candidate. An empty or
/// unreadable primary file blocks its Claude-compatible fallback but adds no
/// prompt bytes.
fn add_opencode_user_file(found: &mut Vec<DiscoveredFile>, roots: &DiscoveryRoots) {
    let candidates = [
        (
            roots.opencode_config.join("AGENTS.md"),
            OverheadFileKind::AgentsMd,
        ),
        (
            roots.home.join(".claude").join("CLAUDE.md"),
            OverheadFileKind::ClaudeMd,
        ),
    ];
    if let Some((path, kind)) = candidates.into_iter().find(|(path, _)| is_file(path)) {
        add_file(
            found,
            kind,
            &path,
            OverheadFileScope::User,
            SourceKind::Opencode,
            usize::MAX,
        );
    }
}

/// Claude Code loads CLAUDE.md + CLAUDE.local.md at every ancestor all the
/// way to the filesystem root. Project-vs-ancestor scope is based on the git
/// root solely for presentation; it does not truncate discovery.
fn add_claude_ancestor_chain(
    found: &mut Vec<DiscoveredFile>,
    project_path: &Path,
    project_scope_root: &Path,
    roots: &DiscoveryRoots,
) {
    let mut claude_chain = Vec::new();
    for dir in project_path.ancestors() {
        claude_chain.push(dir.to_path_buf());
        if roots.claude_ancestor_stop.as_deref() == Some(dir) {
            break;
        }
    }
    claude_chain.reverse();
    for dir in claude_chain {
        let scope = if dir.starts_with(project_scope_root) {
            OverheadFileScope::Project
        } else {
            OverheadFileScope::Ancestor
        };
        let mut paths = vec![(dir.join("CLAUDE.md"), scope)];
        // Official docs name `.claude/CLAUDE.md` as a project-root
        // alternative but do not say it is checked at every ancestor. A
        // scratch session proves git-root discovery from a nested CWD, so we
        // intentionally use the narrow git-root-only rule. Without a git
        // marker, the requested CWD is the project-root fallback.
        if dir == project_scope_root {
            paths.push((
                dir.join(".claude").join("CLAUDE.md"),
                OverheadFileScope::Project,
            ));
        }
        paths.push((dir.join("CLAUDE.local.md"), scope));
        for (path, scope) in paths {
            add_file(
                found,
                OverheadFileKind::ClaudeMd,
                &path,
                scope,
                SourceKind::ClaudeCode,
                usize::MAX,
            );
        }
    }
}

/// Codex chooses at most one candidate per directory by metadata, before it
/// reads content (an empty or unreadable override blocks AGENTS.md in the
/// same directory), and applies one aggregate 32 KiB budget to the
/// root -> CWD project chain.
fn add_codex_project_chain(found: &mut Vec<DiscoveredFile>, project_chain: &[PathBuf]) {
    let mut codex_remaining = DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES;
    for dir in project_chain {
        if codex_remaining == 0 {
            break;
        }
        let Some(path) = ["AGENTS.override.md", "AGENTS.md"]
            .into_iter()
            .map(|name| dir.join(name))
            .find(|path| is_file(path))
        else {
            continue;
        };
        let Some(bytes) = readable_nonempty_bytes(&path) else {
            continue;
        };
        let injected = bytes.len().min(codex_remaining);
        if String::from_utf8_lossy(&bytes[..injected])
            .trim()
            .is_empty()
        {
            continue;
        }
        add_file(
            found,
            OverheadFileKind::AgentsMd,
            &path,
            OverheadFileScope::Project,
            SourceKind::Codex,
            injected,
        );
        codex_remaining -= injected;
    }
}

/// OpenCode takes the first filename class with any existing match, then
/// loads every match in that class from CWD through the worktree root.
fn add_opencode_project_chain(found: &mut Vec<DiscoveredFile>, project_chain: &[PathBuf]) {
    for (name, kind) in [
        ("AGENTS.md", OverheadFileKind::AgentsMd),
        ("CLAUDE.md", OverheadFileKind::ClaudeMd),
    ] {
        let matches: Vec<PathBuf> = project_chain
            .iter()
            .map(|dir| dir.join(name))
            .filter(|path| is_file(path))
            .collect();
        if matches.is_empty() {
            continue;
        }
        for path in matches {
            add_file(
                found,
                kind,
                &path,
                OverheadFileScope::Project,
                SourceKind::Opencode,
                usize::MAX,
            );
        }
        break;
    }
}
