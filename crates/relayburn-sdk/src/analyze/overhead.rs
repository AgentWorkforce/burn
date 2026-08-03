//! Per-file overhead attribution (`CLAUDE.md` for Claude Code, `AGENTS.md`
//! for Codex/OpenCode). Rust port of `packages/analyze/src/overhead.ts`.
//!
//! Composes the `claude_md` parser/attributor over harness-accurate startup
//! instruction chains: each file declares which `SourceKind`s read it into
//! their cached prompt prefix, and only matching turns contribute to that
//! file's cost. The math is `f64` and matches the TS reduce order so the
//! per-file / per-section USD totals stay within the 1e-9 USD precision
//! contract called out in AgentWorkforce/burn#244 and #276.

use std::fs;
use std::path::{Path, PathBuf};

use crate::reader::{SourceKind, TurnRecord};
use serde::{Deserialize, Serialize};

use crate::analyze::claude_md::{
    attribute_claude_md_refs, load_claude_md_file, ClaudeMdAttributionResult, ParsedClaudeMd,
};
use crate::analyze::pricing::PricingTable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverheadFileKind {
    ClaudeMd,
    AgentsMd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverheadFileScope {
    User,
    Ancestor,
    Project,
}

impl OverheadFileScope {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ancestor => "ancestor",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OverheadFile {
    pub kind: OverheadFileKind,
    pub path: String,
    pub scope: OverheadFileScope,
    /// Which agent sources read this file into their cached context. A turn's
    /// `source` must be in this list for the file to count toward that turn.
    pub applies_to: Vec<SourceKind>,
    /// Number of leading bytes the harness injects. This is normally the
    /// complete file, but Codex caps the combined project instruction chain
    /// at 32 KiB and can therefore inject only a prefix of the final file.
    content_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedOverheadFile {
    pub file: OverheadFile,
    pub parsed: ParsedClaudeMd,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OverheadFileAttribution {
    pub file: OverheadFile,
    pub parsed: ParsedClaudeMd,
    pub attribution: ClaudeMdAttributionResult,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OverheadAttribution {
    pub per_file: Vec<OverheadFileAttribution>,
    pub grand_total: f64,
    /// Count of distinct turns that contributed to at least one file's cost.
    /// Not the sum of per-file `riding_turns` — a turn could ride along in
    /// multiple files (e.g. `CLAUDE.md` + `.claude/CLAUDE.md`) and we don't
    /// want to double-count.
    pub total_riding_turns: u64,
}

pub(crate) struct AttributeOverheadInput<'a> {
    pub files: &'a [ParsedOverheadFile],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
}

const DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

#[derive(Debug)]
struct DiscoveryRoots {
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
    fn for_home(home: &Path) -> Self {
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
    if let Some(existing) = out
        .iter_mut()
        .find(|entry| entry.identity == identity && entry.file.kind == kind)
    {
        if !existing.file.applies_to.contains(&source) {
            existing.file.applies_to.push(source);
        }
        // A physical file can be full for one harness but truncated by
        // Codex's aggregate budget. The shorter prefix is conservative for a
        // merged row and can never over-attribute either harness.
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
fn find_overhead_files_with_roots(
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

    // Codex global precedence is first non-empty: override, then AGENTS.md.
    for name in ["AGENTS.override.md", "AGENTS.md"] {
        let path = roots.codex_home.join(name);
        let Some(bytes) = readable_nonempty_bytes(&path) else {
            continue;
        };
        add_file(
            &mut found,
            OverheadFileKind::AgentsMd,
            &path,
            OverheadFileScope::User,
            SourceKind::Codex,
            bytes.len(),
        );
        break;
    }

    // OpenCode stops at the first existing global candidate. An empty or
    // unreadable primary file blocks its Claude-compatible fallback but adds
    // no prompt bytes.
    for path in [
        roots.opencode_config.join("AGENTS.md"),
        roots.home.join(".claude").join("CLAUDE.md"),
    ] {
        if !is_file(&path) {
            continue;
        }
        if let Some(bytes) = readable_nonempty_bytes(&path) {
            let kind = if path.file_name().and_then(|p| p.to_str()) == Some("AGENTS.md") {
                OverheadFileKind::AgentsMd
            } else {
                OverheadFileKind::ClaudeMd
            };
            add_file(
                &mut found,
                kind,
                &path,
                OverheadFileScope::User,
                SourceKind::Opencode,
                bytes.len(),
            );
        }
        break;
    }

    // Claude Code loads CLAUDE.md + CLAUDE.local.md at every ancestor all
    // the way to the filesystem root. Project-vs-ancestor scope is based on
    // the git root solely for presentation; it does not truncate discovery.
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
        let claude_md = dir.join("CLAUDE.md");
        add_file(
            &mut found,
            OverheadFileKind::ClaudeMd,
            &claude_md,
            scope,
            SourceKind::ClaudeCode,
            usize::MAX,
        );
        // Official docs name `.claude/CLAUDE.md` as a project-root
        // alternative but do not say it is checked at every ancestor. A
        // scratch session proves git-root discovery from a nested CWD, so we
        // intentionally use the narrow git-root-only rule. Without a git
        // marker, the requested CWD is the project-root fallback.
        if dir == project_scope_root {
            add_file(
                &mut found,
                OverheadFileKind::ClaudeMd,
                &dir.join(".claude").join("CLAUDE.md"),
                OverheadFileScope::Project,
                SourceKind::ClaudeCode,
                usize::MAX,
            );
        }
        add_file(
            &mut found,
            OverheadFileKind::ClaudeMd,
            &dir.join("CLAUDE.local.md"),
            scope,
            SourceKind::ClaudeCode,
            usize::MAX,
        );
    }

    // Codex chooses at most one non-empty candidate per directory and applies
    // one aggregate 32 KiB budget to the root -> CWD project chain.
    let mut codex_remaining = DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES;
    for dir in &project_chain {
        if codex_remaining == 0 {
            break;
        }
        for name in ["AGENTS.override.md", "AGENTS.md"] {
            let path = dir.join(name);
            if !is_file(&path) {
                continue;
            }
            if let Some(bytes) = readable_nonempty_bytes(&path) {
                let injected = bytes.len().min(codex_remaining);
                if !String::from_utf8_lossy(&bytes[..injected])
                    .trim()
                    .is_empty()
                {
                    add_file(
                        &mut found,
                        OverheadFileKind::AgentsMd,
                        &path,
                        OverheadFileScope::Project,
                        SourceKind::Codex,
                        injected,
                    );
                    codex_remaining -= injected;
                }
            }
            // Codex chooses by metadata before it reads content: an empty or
            // unreadable override blocks AGENTS.md in the same directory.
            break;
        }
    }

    // OpenCode takes the first filename class with any existing match, then
    // loads every match in that class from CWD through the worktree root.
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
                &mut found,
                kind,
                &path,
                OverheadFileScope::Project,
                SourceKind::Opencode,
                usize::MAX,
            );
        }
        break;
    }

    found.into_iter().map(|entry| entry.file).collect()
}

pub(crate) fn load_overhead_file(file: OverheadFile) -> std::io::Result<ParsedOverheadFile> {
    let path = Path::new(&file.path);
    if fs::metadata(path)?.len() as usize <= file.content_bytes {
        let parsed = load_claude_md_file(path)?;
        return Ok(ParsedOverheadFile { file, parsed });
    }
    let mut bytes = fs::read(path)?;
    bytes.truncate(file.content_bytes);
    let text = String::from_utf8_lossy(&bytes);
    let parsed = crate::analyze::claude_md::parse_claude_md(&file.path, &text);
    Ok(ParsedOverheadFile { file, parsed })
}

pub(crate) fn attribute_overhead(input: AttributeOverheadInput<'_>) -> OverheadAttribution {
    let mut per_file: Vec<OverheadFileAttribution> = Vec::new();
    // Per-session max riding-turns across every file. The eviction check is
    // `cache_read >= file_tokens`, so a smaller file's rides are a strict
    // superset of a larger file's rides for the same session+source. Taking
    // the max per session yields the correct count of distinct turns without
    // double-counting when CLAUDE.md and .claude/CLAUDE.md both attribute to
    // the same Claude Code session.
    let mut max_riding_by_session: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    for pf in input.files {
        let filtered: Vec<&TurnRecord> = input
            .turns
            .iter()
            .filter(|t| pf.file.applies_to.contains(&t.source))
            .collect();
        let attribution =
            attribute_claude_md_refs(std::slice::from_ref(&pf.parsed), &filtered, input.pricing);

        for sc in &attribution.session_costs {
            let prev = max_riding_by_session
                .get(&sc.session_id)
                .copied()
                .unwrap_or(0);
            if sc.riding_turns > prev {
                max_riding_by_session.insert(sc.session_id.clone(), sc.riding_turns);
            }
        }

        per_file.push(OverheadFileAttribution {
            file: pf.file.clone(),
            parsed: pf.parsed.clone(),
            attribution,
        });
    }

    let grand_total = per_file.iter().map(|f| f.attribution.total_cost).sum();
    let total_riding_turns = max_riding_by_session.values().sum();

    OverheadAttribution {
        per_file,
        grand_total,
        total_riding_turns,
    }
}

pub fn describe_applies_to(applies_to: &[SourceKind]) -> String {
    let mut as_strs: Vec<&'static str> = applies_to.iter().map(SourceKind::wire_str).collect();
    as_strs.sort_unstable();
    as_strs.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::claude_md::parse_claude_md;
    use crate::analyze::pricing::{ModelCost, ReasoningMode};
    use crate::reader::Usage;

    fn pricing_with(model: &str, cache_read: f64) -> PricingTable {
        let mut p = PricingTable::new();
        p.insert(
            model.to_string(),
            ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read,
                cache_write: 3.75,
                reasoning: None,
                reasoning_mode: ReasoningMode::IncludedInOutput,
                context_tiers: Vec::new(),
            },
        );
        p
    }

    fn mk_turn(session: &str, idx: u64, source: SourceKind, cache_read: u64) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session.to_string(),
            session_path: None,
            message_id: format!("m-{}", idx),
            turn_index: idx,
            ts: "2026-04-23T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 10,
                reasoning: 0,
                cache_read,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn write_fixture(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn mark_git_root(path: &Path) {
        fs::create_dir_all(path.join(".git")).unwrap();
    }

    fn discover_fixture(home: &Path, project: &Path) -> Vec<OverheadFile> {
        find_overhead_files_with_roots(project, &DiscoveryRoots::for_home(home))
    }

    #[test]
    fn discovery_matches_harness_startup_chains_scopes_and_stable_order() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("workspace").join("repo");
        let cwd = root.join("packages").join("api");
        fs::create_dir_all(&cwd).unwrap();
        mark_git_root(&root);

        write_fixture(&home.join(".claude/CLAUDE.md"), "global claude");
        write_fixture(&home.join(".codex/AGENTS.md"), "global codex");
        write_fixture(&home.join(".config/opencode/AGENTS.md"), "global opencode");
        write_fixture(&home.join("CLAUDE.md"), "workspace ancestor");
        write_fixture(&root.join("CLAUDE.md"), "root claude");
        write_fixture(&root.join(".claude/CLAUDE.md"), "dot claude");
        write_fixture(&root.join("CLAUDE.local.md"), "root local");
        write_fixture(&cwd.join("CLAUDE.md"), "cwd claude");
        write_fixture(&cwd.join("CLAUDE.local.md"), "cwd local");
        write_fixture(&root.join("AGENTS.md"), "root agents");
        write_fixture(&cwd.join("AGENTS.md"), "cwd agents");

        let files = discover_fixture(home, &cwd);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                home.join(".claude/CLAUDE.md").to_str().unwrap(),
                home.join(".codex/AGENTS.md").to_str().unwrap(),
                home.join(".config/opencode/AGENTS.md").to_str().unwrap(),
                home.join("CLAUDE.md").to_str().unwrap(),
                root.join("CLAUDE.md").to_str().unwrap(),
                root.join(".claude/CLAUDE.md").to_str().unwrap(),
                root.join("CLAUDE.local.md").to_str().unwrap(),
                cwd.join("CLAUDE.md").to_str().unwrap(),
                cwd.join("CLAUDE.local.md").to_str().unwrap(),
                root.join("AGENTS.md").to_str().unwrap(),
                cwd.join("AGENTS.md").to_str().unwrap(),
            ]
        );

        assert_eq!(files[0].scope, OverheadFileScope::User);
        assert_eq!(files[0].applies_to, vec![SourceKind::ClaudeCode]);
        assert_eq!(files[1].applies_to, vec![SourceKind::Codex]);
        assert_eq!(files[2].applies_to, vec![SourceKind::Opencode]);
        assert_eq!(files[3].scope, OverheadFileScope::Ancestor);
        assert!(files[4..]
            .iter()
            .all(|f| f.scope == OverheadFileScope::Project));
        for file in &files[9..] {
            assert_eq!(
                file.applies_to,
                vec![SourceKind::Codex, SourceKind::Opencode]
            );
        }
    }

    #[test]
    fn codex_and_opencode_stop_at_git_root_but_claude_walks_above_it() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("outer/repo");
        let cwd = root.join("sub");
        fs::create_dir_all(&cwd).unwrap();
        mark_git_root(&root);
        write_fixture(&home.join("outer/AGENTS.md"), "outside agents");
        write_fixture(&home.join("outer/CLAUDE.md"), "outside claude");
        write_fixture(&root.join("AGENTS.md"), "inside agents");

        let files = discover_fixture(home, &cwd);
        let outside_agents = home.join("outer/AGENTS.md").to_string_lossy().into_owned();
        assert!(!files.iter().any(|f| f.path == outside_agents));
        let outside_claude = home.join("outer/CLAUDE.md").to_string_lossy().into_owned();
        let file = files.iter().find(|f| f.path == outside_claude).unwrap();
        assert_eq!(file.scope, OverheadFileScope::Ancestor);
        assert_eq!(file.applies_to, vec![SourceKind::ClaudeCode]);
    }

    #[test]
    fn no_git_root_limits_codex_and_opencode_to_requested_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let parent = home.join("plain");
        let cwd = parent.join("child");
        fs::create_dir_all(&cwd).unwrap();
        write_fixture(&parent.join("AGENTS.md"), "parent agents");
        write_fixture(&cwd.join("AGENTS.md"), "cwd agents");

        let files = discover_fixture(home, &cwd);
        let parent_path = parent.join("AGENTS.md").to_string_lossy().into_owned();
        assert!(!files.iter().any(|f| f.path == parent_path));
        let cwd_path = cwd.join("AGENTS.md").to_string_lossy().into_owned();
        let file = files.iter().find(|f| f.path == cwd_path).unwrap();
        assert_eq!(
            file.applies_to,
            vec![SourceKind::Codex, SourceKind::Opencode]
        );
    }

    #[test]
    fn codex_override_and_opencode_filename_class_precedence_stay_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        fs::create_dir_all(&root).unwrap();
        mark_git_root(&root);
        write_fixture(&root.join("AGENTS.override.md"), "codex override");
        write_fixture(&root.join("AGENTS.md"), "shared agents");
        write_fixture(&root.join("CLAUDE.md"), "claude fallback");

        let files = discover_fixture(home, &root);
        let override_path = root
            .join("AGENTS.override.md")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            files
                .iter()
                .find(|f| f.path == override_path)
                .unwrap()
                .applies_to,
            vec![SourceKind::Codex]
        );
        let agents_path = root.join("AGENTS.md").to_string_lossy().into_owned();
        assert_eq!(
            files
                .iter()
                .find(|f| f.path == agents_path)
                .unwrap()
                .applies_to,
            vec![SourceKind::Opencode]
        );
        let claude_path = root.join("CLAUDE.md").to_string_lossy().into_owned();
        assert_eq!(
            files
                .iter()
                .find(|f| f.path == claude_path)
                .unwrap()
                .applies_to,
            vec![SourceKind::ClaudeCode]
        );
    }

    #[test]
    fn empty_opencode_global_blocks_claude_fallback_without_adding_a_row() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        fs::create_dir_all(&root).unwrap();
        write_fixture(&home.join(".claude/CLAUDE.md"), "claude global");
        write_fixture(&home.join(".config/opencode/AGENTS.md"), "   \n");

        let files = discover_fixture(home, &root);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].applies_to, vec![SourceKind::ClaudeCode]);
        assert_eq!(files[0].scope, OverheadFileScope::User);
    }

    #[test]
    fn empty_codex_project_override_blocks_same_directory_agents_file() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        fs::create_dir_all(&root).unwrap();
        mark_git_root(&root);
        write_fixture(&root.join("AGENTS.override.md"), "  \n");
        write_fixture(&root.join("AGENTS.md"), "opencode still loads this");

        let files = discover_fixture(home, &root);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].applies_to, vec![SourceKind::Opencode]);
    }

    #[test]
    fn inactive_descendants_are_excluded_but_become_active_when_used_as_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        let nested = root.join("services/payments");
        fs::create_dir_all(&nested).unwrap();
        mark_git_root(&root);
        write_fixture(&root.join("CLAUDE.md"), "root");
        write_fixture(&nested.join("CLAUDE.md"), "nested");

        let from_root = discover_fixture(home, &root);
        let nested_path = nested.join("CLAUDE.md").to_string_lossy().into_owned();
        assert!(!from_root.iter().any(|f| f.path == nested_path));

        let from_nested = discover_fixture(home, &nested);
        assert!(from_nested.iter().any(|f| f.path == nested_path));
    }

    #[test]
    fn codex_project_chain_obeys_aggregate_32k_byte_budget() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        let cwd = root.join("sub");
        fs::create_dir_all(&cwd).unwrap();
        mark_git_root(&root);
        write_fixture(
            &root.join("AGENTS.md"),
            &"a".repeat(DEFAULT_CODEX_PROJECT_DOC_MAX_BYTES - 8),
        );
        write_fixture(&cwd.join("AGENTS.md"), "12345678ignored-tail");

        let files = discover_fixture(home, &cwd);
        let cwd_path = cwd.join("AGENTS.md").to_string_lossy().into_owned();
        let file = files.iter().find(|f| f.path == cwd_path).unwrap().clone();
        assert_eq!(file.content_bytes, 8);
        assert_eq!(
            file.applies_to,
            vec![SourceKind::Codex, SourceKind::Opencode]
        );
        let parsed = load_overhead_file(file).unwrap();
        assert_eq!(parsed.parsed.bytes, 8);
    }

    #[cfg(unix)]
    #[test]
    fn physical_identity_deduplicates_claude_symlink_and_hardlink_aliases() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("repo");
        fs::create_dir_all(root.join(".claude")).unwrap();
        mark_git_root(&root);
        write_fixture(&root.join("CLAUDE.md"), "same physical file");
        symlink("../CLAUDE.md", root.join(".claude/CLAUDE.md")).unwrap();
        fs::hard_link(root.join("CLAUDE.md"), root.join("CLAUDE.local.md")).unwrap();

        let files = discover_fixture(home, &root);
        let claude_files: Vec<&OverheadFile> = files
            .iter()
            .filter(|f| f.kind == OverheadFileKind::ClaudeMd)
            .collect();
        assert_eq!(claude_files.len(), 1);
        assert_eq!(
            claude_files[0].applies_to,
            vec![SourceKind::ClaudeCode, SourceKind::Opencode]
        );
    }

    #[test]
    fn routes_turns_by_source_and_grand_total_matches_per_file_sum_within_1e_9() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md =
            parse_claude_md("/p/CLAUDE.md", &format!("## Claude\n{}", "c".repeat(4000)));
        let agents_md =
            parse_claude_md("/p/AGENTS.md", &format!("## Agents\n{}", "a".repeat(4000)));

        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::ClaudeCode],
                    content_bytes: usize::MAX,
                },
                parsed: claude_md.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                    content_bytes: usize::MAX,
                },
                parsed: agents_md.clone(),
            },
        ];

        let turns = vec![
            mk_turn("s-cc", 0, SourceKind::ClaudeCode, claude_md.tokens + 500),
            mk_turn("s-cx", 0, SourceKind::Codex, agents_md.tokens + 500),
            mk_turn("s-oc", 0, SourceKind::Opencode, agents_md.tokens + 500),
        ];

        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.per_file.len(), 2);

        let claude_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::ClaudeMd)
            .unwrap();
        let agents_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::AgentsMd)
            .unwrap();

        // Claude Code session attributes only to CLAUDE.md.
        assert_eq!(claude_attr.attribution.session_count, 1);
        assert_eq!(claude_attr.attribution.session_costs[0].session_id, "s-cc");
        let expected_claude = (claude_md.tokens as f64 / 1_000_000.0) * 0.30;
        assert!(
            (claude_attr.attribution.total_cost - expected_claude).abs() <= expected_claude * 0.10,
            "claude cost={} expected~{}",
            claude_attr.attribution.total_cost,
            expected_claude
        );

        // Agents file attributes to two sessions (codex + opencode).
        assert_eq!(agents_attr.attribution.session_count, 2);
        let expected_agents = 2.0 * (agents_md.tokens as f64 / 1_000_000.0) * 0.30;
        assert!(
            (agents_attr.attribution.total_cost - expected_agents).abs() <= expected_agents * 0.10,
            "agents cost={} expected~{}",
            agents_attr.attribution.total_cost,
            expected_agents
        );

        // 1e-9 USD precision gate: grand_total is the additive sum of per-file
        // total_cost. Same f64 reduce order as the TS implementation.
        let summed = claude_attr.attribution.total_cost + agents_attr.attribution.total_cost;
        assert!((result.grand_total - summed).abs() < 1e-9);
    }

    #[test]
    fn total_riding_turns_takes_max_per_session_not_sum() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let small = parse_claude_md("/p/CLAUDE.md", &format!("## S\n{}", "x".repeat(2000)));
        let big = parse_claude_md(
            "/p/.claude/CLAUDE.md",
            &format!("## B\n{}", "y".repeat(36000)),
        );
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::ClaudeCode],
                    content_bytes: usize::MAX,
                },
                parsed: small.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/.claude/CLAUDE.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::ClaudeCode],
                    content_bytes: usize::MAX,
                },
                parsed: big.clone(),
            },
        ];
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..5 {
            turns.push(mk_turn(
                "s-both",
                i,
                SourceKind::ClaudeCode,
                big.tokens + 1000,
            ));
        }
        for i in 5..8 {
            turns.push(mk_turn(
                "s-both",
                i,
                SourceKind::ClaudeCode,
                small.tokens + 500,
            ));
        }
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let small_attr = result
            .per_file
            .iter()
            .find(|p| p.file.path == "/p/CLAUDE.md")
            .unwrap();
        let big_attr = result
            .per_file
            .iter()
            .find(|p| p.file.path == "/p/.claude/CLAUDE.md")
            .unwrap();
        assert_eq!(small_attr.attribution.session_costs[0].riding_turns, 8);
        assert_eq!(big_attr.attribution.session_costs[0].riding_turns, 5);
        // Correct: max(8, 5) == 8 (NOT 13).
        assert_eq!(result.total_riding_turns, 8);
    }

    #[test]
    fn does_not_cross_attribute_codex_to_claude_md() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md = parse_claude_md("/p/CLAUDE.md", &format!("## C\n{}", "x".repeat(4000)));
        let agents_md = parse_claude_md("/p/AGENTS.md", &format!("## A\n{}", "y".repeat(4000)));
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::ClaudeCode],
                    content_bytes: usize::MAX,
                },
                parsed: claude_md,
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                    content_bytes: usize::MAX,
                },
                parsed: agents_md,
            },
        ];
        let turns = vec![mk_turn("s-cx", 0, SourceKind::Codex, 50_000)];
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let claude_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::ClaudeMd)
            .unwrap();
        assert_eq!(claude_attr.attribution.total_cost, 0.0);
        assert_eq!(claude_attr.attribution.session_count, 0);
    }

    #[test]
    fn load_overhead_file_round_trips_via_find() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("AGENTS.md"), "## Section\nbody").unwrap();
        let files = discover_fixture(dir.path(), &root);
        assert_eq!(files.len(), 1);
        let f = files.into_iter().next().unwrap();
        let parsed = load_overhead_file(f).unwrap();
        assert_eq!(parsed.parsed.sections[0].heading, "## Section");
    }

    #[test]
    fn describe_applies_to_returns_sorted_csv() {
        assert_eq!(
            describe_applies_to(&[SourceKind::Opencode, SourceKind::Codex]),
            "codex, opencode"
        );
        assert_eq!(
            describe_applies_to(&[SourceKind::ClaudeCode]),
            "claude-code"
        );
    }

    #[test]
    fn per_section_attribution_matches_grand_total_within_1e_9() {
        // Multi-file fixture corpus: per-file, per-section USD totals must
        // sum to the grand total within 1e-9 USD.
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md = parse_claude_md(
            "/p/CLAUDE.md",
            "## Alpha\nalpha-body alpha-body alpha-body\n\n## Beta\nbeta beta beta beta beta\n",
        );
        let agents_md = parse_claude_md(
            "/p/AGENTS.md",
            "## Gamma\ngamma gamma\n\n## Delta\ndelta delta delta\n",
        );
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::ClaudeCode],
                    content_bytes: usize::MAX,
                },
                parsed: claude_md.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    scope: OverheadFileScope::Project,
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                    content_bytes: usize::MAX,
                },
                parsed: agents_md.clone(),
            },
        ];
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            turns.push(mk_turn(
                "s-cc",
                i,
                SourceKind::ClaudeCode,
                claude_md.tokens + 1000,
            ));
        }
        for i in 0..2 {
            turns.push(mk_turn(
                "s-cx",
                i,
                SourceKind::Codex,
                agents_md.tokens + 1000,
            ));
        }
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });

        // For each per-file attribution, Σ section.total_cost ≤ file.total_cost
        // (≤ because section share is byte-additive while file tokens are
        // ceil-rounded). The grand total across files matches the additive
        // sum of per-file costs to within 1e-9 USD.
        let mut summed_per_file = 0.0_f64;
        for fa in &result.per_file {
            summed_per_file += fa.attribution.total_cost;
            let sec_sum: f64 = fa
                .attribution
                .section_costs
                .iter()
                .map(|s| s.total_cost)
                .sum();
            assert!(sec_sum <= fa.attribution.total_cost + 1e-9);
        }
        assert!((result.grand_total - summed_per_file).abs() < 1e-9);
    }
}
