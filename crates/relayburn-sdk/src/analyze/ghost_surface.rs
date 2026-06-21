//! Cross-harness ghost-user-installed-surface detector. Rust port of
//! `packages/analyze/src/ghost-surface.ts` — see AgentWorkforce/burn#273.
//!
//! User-installed surface files (agents, skills, commands, prompts, rules,
//! memories) ride in every session's system prompt. When the user has
//! authored a file but the agent never invokes it, the file is dead weight on
//! every API call — the same fixed token cost paid on every session for zero
//! utility. This detector enumerates those files per harness and
//! cross-references basenames against observed tool-call / agent / command /
//! prompt names in the user's session history.
//!
//! Per-harness adapters (Claude / Codex / OpenCode) keep the logic
//! declarative. Adding a new harness is one new `GhostSurfaceAdapter` impl
//! plus a registry entry. The top-level `detect_ghost_surface` orchestrator
//! runs each adapter on its own filesystem surface and folds the results into
//! a single `Vec<GhostSurfaceFinding>`.
//!
//! OpenCode dedup vs. catalog-bloat: the OpenCode catalog-bloat detector
//! (`SystemPromptTax`) already attributes the cost of the declared skill
//! catalog as a per-session fixed tax. To avoid double-counting, ghost
//! candidates whose basenames appear in the declared catalog set are still
//! surfaced (so the user knows what to remove) but emitted with `cost: 0`.
//!
//! Slash-command-style invocations (e.g. a user typing `/openspec-archive`
//! in the UI) are NOT recorded as tool calls and won't appear in
//! `observedNamesBySource`. To close the gap, each adapter optionally
//! implements `observed_names`: the orchestrator unions whatever extra names
//! that returns into the observed-names set before filtering candidates. The
//! Claude and Codex adapters use that hook to mine `userTurnTextBySession`
//! for slash-command markers (`<command-name>` blocks for Claude, literal
//! `/<basename>` matches for Codex). The map is source-keyed first so each
//! adapter only sees its own source's text — without that scoping, a Claude
//! `<command-name>/foo</command-name>` marker would de-ghost an
//! identically-named Codex prompt.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::reader::SourceKind;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::analyze::findings::{EstimatedSavings, WasteAction, WasteFinding};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Closed enum of finding kinds. Serializes to the same kebab-case strings
/// as the TS string-literal union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(clippy::enum_variant_names)]
pub enum GhostFindingKind {
    GhostAgent,
    GhostSkill,
    GhostCommand,
    GhostPrompt,
    GhostRule,
    GhostMemory,
}

impl GhostFindingKind {
    fn as_kebab(self) -> &'static str {
        match self {
            GhostFindingKind::GhostAgent => "ghost-agent",
            GhostFindingKind::GhostSkill => "ghost-skill",
            GhostFindingKind::GhostCommand => "ghost-command",
            GhostFindingKind::GhostPrompt => "ghost-prompt",
            GhostFindingKind::GhostRule => "ghost-rule",
            GhostFindingKind::GhostMemory => "ghost-memory",
        }
    }

    fn bare_name(self) -> &'static str {
        // Strips the `ghost-` prefix for display.
        match self {
            GhostFindingKind::GhostAgent => "agent",
            GhostFindingKind::GhostSkill => "skill",
            GhostFindingKind::GhostCommand => "command",
            GhostFindingKind::GhostPrompt => "prompt",
            GhostFindingKind::GhostRule => "rule",
            GhostFindingKind::GhostMemory => "memory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhostSurfaceFinding {
    pub source: SourceKind,
    pub kind: GhostFindingKind,
    pub path: String,
    pub size_tokens: u64,
    pub cost: f64,
    pub cost_per_session: f64,
    pub session_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_by_catalog_bloat: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhostCandidate {
    pub source: SourceKind,
    pub kind: GhostFindingKind,
    pub path: String,
    pub basename: String,
    pub size_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_by_catalog_bloat: Option<bool>,
}

/// Inputs to `detect_ghost_surface`. Mirrors the TS `GhostSurfaceInputs`.
///
/// Keys in the cross-source maps are `SourceKind`. `userTurnTextBySession`
/// is keyed first by source, then by sessionId, then a list of raw user-turn
/// text bodies. Optional fields default to absent.
#[derive(Debug, Clone, Default)]
pub struct GhostSurfaceInputs {
    pub observed_names_by_source: HashMap<SourceKind, HashSet<String>>,
    pub session_count_by_source: HashMap<SourceKind, u64>,
    pub dollar_per_token: f64,
    pub claude_home: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub opencode_projects: Option<Vec<PathBuf>>,
    /// Optional: per-source, per-session list of user-turn text. When
    /// `None` (or for sources missing from the outer map) the slash-command
    /// observation pass is skipped and the detector falls back to v1
    /// (tool-call only) behaviour.
    pub user_turn_text_by_session: Option<HashMap<SourceKind, HashMap<String, Vec<String>>>>,
}

/// Adapter trait. Each harness implements `enumerate` to walk its
/// filesystem surface, and may optionally implement `observed_names` to
/// contribute extra observations from `user_turn_text_by_session`.
pub trait GhostSurfaceAdapter: Sync {
    fn source(&self) -> SourceKind;

    fn enumerate(&self, inputs: &GhostSurfaceInputs) -> Vec<GhostCandidate>;

    /// Optional. Default returns an empty set. Called only when this
    /// adapter's source has at least one session's worth of text in
    /// `user_turn_text_by_session` — see the per-source-scoping note in the
    /// module doc.
    fn observed_names(
        &self,
        _inputs: &GhostSurfaceInputs,
        _candidates: &[GhostCandidate],
    ) -> HashSet<String> {
        HashSet::new()
    }
}

#[derive(Debug, Default, Clone)]
pub struct GhostSurfaceFindingOptions {
    pub archive_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

const APPROX_BYTES_PER_TOKEN: u64 = 4;

fn approx_tokens_from_bytes(byte_len: u64) -> u64 {
    // Math.ceil(byteLen / 4)
    byte_len.div_ceil(APPROX_BYTES_PER_TOKEN)
}

fn strip_extension(basename: &str) -> &str {
    match basename.rfind('.') {
        Some(i) if i > 0 => &basename[..i],
        _ => basename,
    }
}

/// Returns the raw stem and a lowercase variant. Some harnesses normalize
/// tool names with mixed case; we match case-insensitively to be forgiving.
fn names_for_lookup(basename: &str) -> Vec<String> {
    let stem = strip_extension(basename).to_string();
    let lower = stem.to_lowercase();
    if stem == lower {
        vec![stem]
    } else {
        vec![stem, lower]
    }
}

#[derive(Debug, Clone)]
struct DirEntry {
    path: PathBuf,
    basename: String,
    size: u64,
}

/// Walk a directory non-recursively. Returns regular files matching
/// `predicate`; returns `[]` when the directory doesn't exist. Mirrors
/// `listDirFiles` in TS.
fn list_dir_files<F>(dir: &Path, predicate: F) -> Vec<DirEntry>
where
    F: Fn(&str) -> bool,
{
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            // ENOENT / ENOTDIR: surface no entries.
            if err.kind() == std::io::ErrorKind::NotFound
                || err.kind() == std::io::ErrorKind::NotADirectory
            {
                return Vec::new();
            }
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !predicate(&name) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(DirEntry {
            path: entry.path(),
            basename: name,
            size: metadata.len(),
        });
    }
    // Iteration order from fs::read_dir is filesystem-dependent. Sort by
    // basename for deterministic output across platforms (matches the
    // `Object.keys` insertion-order assumption in the TS tests when the
    // tests sort the output themselves; for paths that fall through to
    // the cost-desc final sort, this just stabilizes ties).
    out.sort_by(|a, b| a.basename.cmp(&b.basename));
    out
}

fn is_markdown(name: &str) -> bool {
    name.ends_with(".md") || name.ends_with(".markdown")
}

fn is_plain_text_surface(name: &str) -> bool {
    if name.starts_with('.') {
        return false;
    }
    // Conservative deny-list: anything that's clearly not a prompt/skill/rule.
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".json",
        ".jsonl",
        ".yaml",
        ".yml",
        ".toml",
        ".lock",
        ".log",
        ".tsbuildinfo",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".webp",
        ".pdf",
        ".zip",
        ".tar",
        ".gz",
    ] {
        if lower.ends_with(ext) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Slash-command observation
// ---------------------------------------------------------------------------

// `<command-name>...</command-name>` — accept optional leading `/` and
// trailing whitespace inside the wrapper. Capture the first non-space
// chunk inside.
static CLAUDE_COMMAND_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<command-name>\s*/?([\w./:\-]+?)\s*</command-name>").unwrap()
});

pub fn mine_claude_command_names(
    user_turn_text_by_session: Option<&HashMap<String, Vec<String>>>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let map = match user_turn_text_by_session {
        Some(m) if !m.is_empty() => m,
        _ => return out,
    };
    let re = &*CLAUDE_COMMAND_NAME_RE;
    for texts in map.values() {
        for text in texts {
            if text.is_empty() {
                continue;
            }
            for cap in re.captures_iter(text) {
                if let Some(raw) = cap.get(1) {
                    let raw = raw.as_str();
                    // Strip a trailing arg list (`foo args`) — defensive.
                    let head = raw.split_whitespace().next().unwrap_or("");
                    if !head.is_empty() {
                        out.insert(head.to_string());
                    }
                }
            }
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn mine_codex_slash_invocations(
    user_turn_text_by_session: Option<&HashMap<String, Vec<String>>>,
    candidates: &[GhostCandidate],
) -> HashSet<String> {
    let mut out = HashSet::new();
    let map = match user_turn_text_by_session {
        Some(m) if !m.is_empty() => m,
        _ => return out,
    };
    // Build a stem -> raw-name lookup so we can return the original basename
    // form. Stems are lower-cased for the search.
    let mut stems: BTreeMap<String, String> = BTreeMap::new();
    for cand in candidates {
        let stem = strip_extension(&cand.basename);
        if stem.is_empty() {
            continue;
        }
        stems.insert(stem.to_lowercase(), stem.to_string());
    }
    if stems.is_empty() {
        return out;
    }
    for texts in map.values() {
        for text in texts {
            if text.is_empty() {
                continue;
            }
            let lower = text.to_lowercase();
            for (stem_lower, stem_original) in &stems {
                // Find the first valid `/<stem>` match with proper word
                // boundaries on both sides.
                let needle = format!("/{stem_lower}");
                let mut from = 0usize;
                let mut matched = false;
                while from <= lower.len() {
                    let Some(rel) = lower[from..].find(&needle) else {
                        break;
                    };
                    let idx = from + rel;
                    // Left boundary: char before `/` must NOT be a word char.
                    if idx > 0 {
                        let left = lower[..idx].chars().next_back().unwrap();
                        if is_word_char(left) {
                            from = idx + 1;
                            continue;
                        }
                    }
                    // Right boundary: char after stem must not be a word
                    // char or a hyphen.
                    let after = idx + needle.len();
                    if after < lower.len() {
                        let right = lower[after..].chars().next().unwrap();
                        if is_word_char(right) || right == '-' {
                            from = idx + 1;
                            continue;
                        }
                    }
                    out.insert(stem_original.clone());
                    matched = true;
                    break;
                }
                let _ = matched;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Adapters
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ClaudeGhostAdapter;

impl GhostSurfaceAdapter for ClaudeGhostAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::ClaudeCode
    }

    fn enumerate(&self, inputs: &GhostSurfaceInputs) -> Vec<GhostCandidate> {
        let home = inputs
            .claude_home
            .clone()
            .unwrap_or_else(|| home_dir().join(".claude"));
        let mut out = Vec::new();
        let surfaces: &[(GhostFindingKind, &str)] = &[
            (GhostFindingKind::GhostAgent, "agents"),
            (GhostFindingKind::GhostSkill, "skills"),
            (GhostFindingKind::GhostCommand, "commands"),
        ];
        for (kind, sub) in surfaces {
            let dir = home.join(sub);
            for file in list_dir_files(&dir, is_markdown) {
                out.push(GhostCandidate {
                    source: SourceKind::ClaudeCode,
                    kind: *kind,
                    path: file.path.to_string_lossy().to_string(),
                    basename: file.basename,
                    size_tokens: approx_tokens_from_bytes(file.size),
                    counted_by_catalog_bloat: None,
                });
            }
        }
        out
    }

    fn observed_names(
        &self,
        inputs: &GhostSurfaceInputs,
        _candidates: &[GhostCandidate],
    ) -> HashSet<String> {
        let texts = inputs
            .user_turn_text_by_session
            .as_ref()
            .and_then(|m| m.get(&SourceKind::ClaudeCode));
        mine_claude_command_names(texts)
    }
}

#[derive(Debug, Default)]
pub struct CodexGhostAdapter;

impl GhostSurfaceAdapter for CodexGhostAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Codex
    }

    fn enumerate(&self, inputs: &GhostSurfaceInputs) -> Vec<GhostCandidate> {
        let home = inputs
            .codex_home
            .clone()
            .unwrap_or_else(|| home_dir().join(".codex"));
        let mut out = Vec::new();
        let surfaces: &[(GhostFindingKind, &str)] = &[
            (GhostFindingKind::GhostPrompt, "prompts"),
            (GhostFindingKind::GhostSkill, "skills"),
            (GhostFindingKind::GhostRule, "rules"),
            (GhostFindingKind::GhostMemory, "memories"),
        ];
        for (kind, sub) in surfaces {
            let dir = home.join(sub);
            for file in list_dir_files(&dir, is_plain_text_surface) {
                out.push(GhostCandidate {
                    source: SourceKind::Codex,
                    kind: *kind,
                    path: file.path.to_string_lossy().to_string(),
                    basename: file.basename,
                    size_tokens: approx_tokens_from_bytes(file.size),
                    counted_by_catalog_bloat: None,
                });
            }
        }
        out
    }

    fn observed_names(
        &self,
        inputs: &GhostSurfaceInputs,
        candidates: &[GhostCandidate],
    ) -> HashSet<String> {
        let texts = inputs
            .user_turn_text_by_session
            .as_ref()
            .and_then(|m| m.get(&SourceKind::Codex));
        mine_codex_slash_invocations(texts, candidates)
    }
}

#[derive(Debug, Default)]
pub struct OpenCodeGhostAdapter;

impl GhostSurfaceAdapter for OpenCodeGhostAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Opencode
    }

    fn enumerate(&self, inputs: &GhostSurfaceInputs) -> Vec<GhostCandidate> {
        let projects: Vec<PathBuf> = match &inputs.opencode_projects {
            Some(v) if !v.is_empty() => v.clone(),
            _ => vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))],
        };
        let mut out = Vec::new();
        for project in projects {
            out.extend(enumerate_opencode_project(&project));
        }
        out
    }
}

fn enumerate_opencode_project(project: &Path) -> Vec<GhostCandidate> {
    let mut out = Vec::new();
    let mut declared_skills: BTreeSet<String> = BTreeSet::new();
    let mut declared_commands: Vec<(String, u64, String)> = Vec::new();

    let config_path = project.join("opencode.json");
    let config_path_str = config_path.to_string_lossy().to_string();

    // Match TS: ENOENT silently returns no config; any other error also
    // falls through (the TS path bubbles the error, but we'd rather
    // tolerate odd permission failures and continue scanning project
    // skill folders).
    let config_raw = fs::read_to_string(&config_path).ok();

    if let Some(raw) = config_raw {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(obj) = parsed.as_object() {
                if let Some(skills) = obj.get("skills") {
                    if let Some(map) = skills.as_object() {
                        for k in map.keys() {
                            declared_skills.insert(k.clone());
                        }
                    } else if let Some(arr) = skills.as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                declared_skills.insert(s.to_string());
                            }
                        }
                    }
                }
                if let Some(commands) = obj.get("commands") {
                    if let Some(map) = commands.as_object() {
                        for (name, val) in map {
                            // Match TS `JSON.stringify(val ?? {})`: serialize
                            // null as `{}`, everything else verbatim. Keys
                            // come from a serde_json Map which preserves
                            // insertion order (when the `preserve_order`
                            // feature is enabled) — `indexmap` is in
                            // workspace deps but the default `Map` is the
                            // BTreeMap-based one in stable serde_json.
                            let serialized = if val.is_null() {
                                "{}".to_string()
                            } else {
                                serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string())
                            };
                            declared_commands.push((
                                name.clone(),
                                approx_tokens_from_bytes(serialized.len() as u64),
                                format!("{config_path_str}#/commands/{name}"),
                            ));
                        }
                    }
                }
            }
        }
        // Malformed config — silently skip catalog-bloat dedup.
    }

    for name in &declared_skills {
        out.push(GhostCandidate {
            source: SourceKind::Opencode,
            kind: GhostFindingKind::GhostSkill,
            path: format!("{config_path_str}#/skills/{name}"),
            basename: name.clone(),
            size_tokens: 0,
            counted_by_catalog_bloat: Some(true),
        });
    }

    for (name, size_tokens, path) in declared_commands {
        out.push(GhostCandidate {
            source: SourceKind::Opencode,
            kind: GhostFindingKind::GhostCommand,
            path,
            basename: name,
            size_tokens,
            counted_by_catalog_bloat: None,
        });
    }

    let skill_dirs = [
        project.join(".opencode").join("skills"),
        project.join("skills"),
    ];
    for dir in &skill_dirs {
        for file in list_dir_files(dir, is_markdown) {
            out.push(GhostCandidate {
                source: SourceKind::Opencode,
                kind: GhostFindingKind::GhostSkill,
                path: file.path.to_string_lossy().to_string(),
                basename: file.basename,
                size_tokens: approx_tokens_from_bytes(file.size),
                counted_by_catalog_bloat: None,
            });
        }
    }

    out
}

/// Default adapter registry. Mirrors `DEFAULT_GHOST_ADAPTERS` in TS.
pub fn default_ghost_adapters() -> Vec<Box<dyn GhostSurfaceAdapter>> {
    vec![
        Box::new(ClaudeGhostAdapter),
        Box::new(CodexGhostAdapter),
        Box::new(OpenCodeGhostAdapter),
    ]
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

pub fn detect_ghost_surface(inputs: &GhostSurfaceInputs) -> Vec<GhostSurfaceFinding> {
    let adapters = default_ghost_adapters();
    let refs: Vec<&dyn GhostSurfaceAdapter> = adapters.iter().map(|a| a.as_ref()).collect();
    detect_ghost_surface_with_adapters(inputs, &refs)
}

pub fn detect_ghost_surface_with_adapters(
    inputs: &GhostSurfaceInputs,
    adapters: &[&dyn GhostSurfaceAdapter],
) -> Vec<GhostSurfaceFinding> {
    let mut out: Vec<GhostSurfaceFinding> = Vec::new();
    for adapter in adapters {
        let candidates = adapter.enumerate(inputs);
        let observed_raw_default = HashSet::new();
        let observed_raw = inputs
            .observed_names_by_source
            .get(&adapter.source())
            .unwrap_or(&observed_raw_default);
        // Build a lower-cased lookup set so comparisons are case-insensitive
        // without forcing callers to pre-normalize their observed-names input.
        let mut observed_lower: HashSet<String> =
            observed_raw.iter().map(|n| n.to_lowercase()).collect();

        // Adapter-local observation pass (slash-command miners). Only
        // invoke when this adapter's source has at least one session's
        // worth of text — per-source scoping prevents cross-harness
        // contamination.
        let source_texts = inputs
            .user_turn_text_by_session
            .as_ref()
            .and_then(|m| m.get(&adapter.source()));
        if let Some(texts) = source_texts {
            if !texts.is_empty() {
                let extra = adapter.observed_names(inputs, &candidates);
                for name in extra {
                    observed_lower.insert(name.to_lowercase());
                }
            }
        }

        let session_count = inputs
            .session_count_by_source
            .get(&adapter.source())
            .copied()
            .unwrap_or(0);

        for cand in candidates {
            let lookups = names_for_lookup(&cand.basename);
            let is_invoked = lookups
                .iter()
                .any(|n| observed_lower.contains(&n.to_lowercase()));
            if is_invoked {
                continue;
            }
            let counted = cand.counted_by_catalog_bloat.unwrap_or(false);
            let cost_per_session = if counted {
                0.0
            } else {
                cand.size_tokens as f64 * inputs.dollar_per_token
            };
            let cost = if counted {
                0.0
            } else {
                cost_per_session * session_count as f64
            };
            out.push(GhostSurfaceFinding {
                source: cand.source,
                kind: cand.kind,
                path: cand.path,
                size_tokens: cand.size_tokens,
                cost,
                cost_per_session,
                session_count,
                counted_by_catalog_bloat: cand.counted_by_catalog_bloat,
            });
        }
    }
    // Sort by cost descending, then size descending, then path ascending for
    // determinism — matches the TS comparator.
    out.sort_by(|a, b| {
        match b
            .cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => {}
            o => return o,
        }
        match b.size_tokens.cmp(&a.size_tokens) {
            std::cmp::Ordering::Equal => {}
            o => return o,
        }
        a.path.cmp(&b.path)
    });
    out
}

// ---------------------------------------------------------------------------
// Finding envelope adapter
// ---------------------------------------------------------------------------

use super::findings::severity_from_usd;

fn default_archive_dir() -> PathBuf {
    crate::ledger::ledger_home().join("ghost-archive")
}

fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(".")
}

/// POSIX shell single-quote escape: wrap in single quotes and replace each
/// embedded `'` with `'\''`. Matches the TS `shellQuote` helper.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Debug, Clone)]
struct SyntheticPath {
    file: String,
    pointer: String,
}

fn split_synthetic_path(p: &str) -> Option<SyntheticPath> {
    let hash = p.find('#')?;
    Some(SyntheticPath {
        file: p[..hash].to_string(),
        pointer: p[hash + 1..].to_string(),
    })
}

fn basename_of(path: &str) -> String {
    // POSIX-style basename (matches `path.basename` on Unix). Also matches
    // the test invariant where synthetic paths produce a final segment after
    // the last `/`.
    Path::new(path)
        .file_name()
        .map(|os| os.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

use super::util::{fmt_usd, format_with_commas};

pub fn ghost_surface_to_finding(
    ghost: &GhostSurfaceFinding,
    options: &GhostSurfaceFindingOptions,
) -> WasteFinding {
    let archive_dir = options
        .archive_dir
        .clone()
        .unwrap_or_else(default_archive_dir);
    let archive_dir_str = archive_dir.to_string_lossy().to_string();
    let synthetic = split_synthetic_path(&ghost.path);
    let kind_label = ghost.kind.as_kebab();
    let action = match &synthetic {
        Some(s) => {
            let file_basename = basename_of(&s.file);
            WasteAction::Paste {
                label: format!("Remove ghost {kind_label} from {file_basename}"),
                text: format!("Edit {} and remove the entry at {}.", s.file, s.pointer),
            }
        }
        None => {
            let archive_with_slash = format!("{archive_dir_str}/");
            WasteAction::Command {
                label: format!("Archive ghost {kind_label}"),
                text: format!(
                    "mkdir -p {} && mv {} {}",
                    shell_quote(&archive_dir_str),
                    shell_quote(&ghost.path),
                    shell_quote(&archive_with_slash),
                ),
            }
        }
    };
    let per_session_usd = ghost.cost_per_session;
    let severity = severity_from_usd(per_session_usd);
    let session_id = format!("ghost:{}", ghost.path);
    let dedup_note = if ghost.counted_by_catalog_bloat == Some(true) {
        " Cost is reported as $0 here because the OpenCode catalog-bloat detector already attributes this entry — see `burn hotspots --patterns opencode-system-prompt`."
    } else {
        ""
    };
    let sessions_clause = if ghost.session_count > 0 {
        let source_str = source_kind_str(ghost.source);
        format!(
            " Observed across {n} {src} session(s) in the lookback window.",
            n = ghost.session_count,
            src = source_str,
        )
    } else {
        String::new()
    };
    let title_basename = basename_of(&ghost.path);
    let title_basename = title_basename.split('#').next().unwrap_or(&title_basename);
    let source_str = source_kind_str(ghost.source);
    WasteFinding {
        kind: kind_label.to_string(),
        severity,
        session_id,
        title: format!(
            "Ghost {bare}: {basename} ({source})",
            bare = ghost.kind.bare_name(),
            basename = title_basename,
            source = source_str,
        ),
        detail: format!(
            "{path} is part of the user-installed {source} surface (~{tokens} tokens) but its basename was never invoked as a tool / agent / command / prompt in the observed window.{sessions} Per-session cost {per}; cumulative {cum}.{dedup}",
            path = ghost.path,
            source = source_str,
            tokens = format_with_commas(ghost.size_tokens),
            sessions = sessions_clause,
            per = fmt_usd(per_session_usd),
            cum = fmt_usd(ghost.cost),
            dedup = dedup_note,
        ),
        estimated_savings: EstimatedSavings {
            tokens_per_session: Some(ghost.size_tokens),
            usd_per_session: Some(per_session_usd),
            usd_per_month: None,
        },
        actions: vec![action],
        event_source: None,
    }
}

fn source_kind_str(source: SourceKind) -> &'static str {
    match source {
        SourceKind::ClaudeCode => "claude-code",
        SourceKind::Codex => "codex",
        SourceKind::Opencode => "opencode",
        SourceKind::AnthropicApi => "anthropic-api",
        SourceKind::OpenaiApi => "openai-api",
        SourceKind::GeminiApi => "gemini-api",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::findings::WasteSeverity;
    use std::path::PathBuf;

    fn fixtures_root() -> PathBuf {
        // crates/relayburn-analyze/Cargo.toml -> repo root is two levels up.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests")
            .join("fixtures")
            .join("ghost-surface")
    }

    fn claude_home() -> PathBuf {
        fixtures_root().join("claude")
    }
    fn codex_home() -> PathBuf {
        fixtures_root().join("codex")
    }
    fn opencode_project() -> PathBuf {
        fixtures_root().join("opencode-project")
    }

    const RATE: f64 = 1e-6;

    fn make_inputs() -> GhostSurfaceInputs {
        GhostSurfaceInputs {
            observed_names_by_source: HashMap::new(),
            session_count_by_source: HashMap::new(),
            dollar_per_token: RATE,
            claude_home: Some(claude_home()),
            codex_home: Some(codex_home()),
            opencode_projects: Some(vec![opencode_project()]),
            user_turn_text_by_session: None,
        }
    }

    fn observed(source: SourceKind, names: &[&str]) -> HashMap<SourceKind, HashSet<String>> {
        let mut m = HashMap::new();
        m.insert(source, names.iter().map(|s| s.to_string()).collect());
        m
    }

    fn observed_multi(entries: &[(SourceKind, &[&str])]) -> HashMap<SourceKind, HashSet<String>> {
        let mut m = HashMap::new();
        for (s, names) in entries {
            m.insert(*s, names.iter().map(|s| s.to_string()).collect());
        }
        m
    }

    fn count_map(entries: &[(SourceKind, u64)]) -> HashMap<SourceKind, u64> {
        entries.iter().copied().collect()
    }

    type UserTextEntries = Vec<(SourceKind, Vec<(String, Vec<String>)>)>;

    fn user_text(entries: UserTextEntries) -> HashMap<SourceKind, HashMap<String, Vec<String>>> {
        let mut out = HashMap::new();
        for (src, sessions) in entries {
            let mut inner = HashMap::new();
            for (sid, texts) in sessions {
                inner.insert(sid, texts);
            }
            out.insert(src, inner);
        }
        out
    }

    // ---- claudeGhostAdapter --------------------------------------------------

    #[test]
    fn claude_enumerates_agents_skills_commands() {
        let candidates = ClaudeGhostAdapter.enumerate(&make_inputs());
        let kinds: HashSet<GhostFindingKind> = candidates.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&GhostFindingKind::GhostAgent), "has agents");
        assert!(kinds.contains(&GhostFindingKind::GhostSkill), "has skills");
        assert!(
            kinds.contains(&GhostFindingKind::GhostCommand),
            "has commands"
        );
        let mut agents: Vec<String> = candidates
            .iter()
            .filter(|c| c.kind == GhostFindingKind::GhostAgent)
            .map(|c| c.basename.clone())
            .collect();
        agents.sort();
        assert_eq!(agents, vec!["code-reviewer.md", "forgotten-helper.md"]);
    }

    #[test]
    fn claude_returns_empty_when_home_missing() {
        let mut inputs = make_inputs();
        inputs.claude_home = Some(fixtures_root().join("does-not-exist"));
        let candidates = ClaudeGhostAdapter.enumerate(&inputs);
        assert_eq!(candidates.len(), 0);
    }

    #[test]
    fn claude_detects_ghost_agent_when_basename_not_observed() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source =
            observed(SourceKind::ClaudeCode, &["code-reviewer", "git-commit"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 10)]);
        let ghosts = detect_ghost_surface(&inputs);
        let claude_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::ClaudeCode)
            .collect();
        let mut basenames: Vec<String> =
            claude_ghosts.iter().map(|g| basename_of(&g.path)).collect();
        basenames.sort();
        assert_eq!(
            basenames,
            vec![
                "forgotten-helper.md",
                "openspec-apply.md",
                "openspec-archive.md",
            ]
        );
        let helper = claude_ghosts
            .iter()
            .find(|g| g.path.ends_with("forgotten-helper.md"))
            .unwrap();
        assert_eq!(helper.kind, GhostFindingKind::GhostAgent);
        assert_eq!(helper.session_count, 10);
        assert!(helper.cost > 0.0);
        assert!(helper.size_tokens > 0);
    }

    #[test]
    fn claude_de_ghosts_command_via_slash_form() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source =
            observed(SourceKind::ClaudeCode, &["code-reviewer", "git-commit"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 10)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::ClaudeCode,
            vec![(
                "session-1".to_string(),
                vec![
                    "<command-name>/openspec-apply</command-name>\nApply the latest proposal."
                        .to_string(),
                ],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let mut basenames: Vec<String> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::ClaudeCode)
            .map(|g| basename_of(&g.path))
            .collect();
        basenames.sort();
        assert_eq!(
            basenames,
            vec!["forgotten-helper.md", "openspec-archive.md"]
        );
    }

    #[test]
    fn claude_recognises_bare_command_name_no_leading_slash() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source =
            observed(SourceKind::ClaudeCode, &["code-reviewer", "git-commit"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::ClaudeCode,
            vec![(
                "session-1".to_string(),
                vec!["<command-name>openspec-apply</command-name>\nbody".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::ClaudeCode && g.path.ends_with("openspec-apply.md"));
        assert!(
            apply.is_none(),
            "claude openspec-apply should be de-ghosted"
        );
    }

    #[test]
    fn claude_falls_back_to_v1_when_user_text_empty() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source =
            observed(SourceKind::ClaudeCode, &["code-reviewer", "git-commit"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 10)]);
        inputs.user_turn_text_by_session = Some(HashMap::new());
        let ghosts = detect_ghost_surface(&inputs);
        let mut basenames: Vec<String> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::ClaudeCode)
            .map(|g| basename_of(&g.path))
            .collect();
        basenames.sort();
        assert_eq!(
            basenames,
            vec![
                "forgotten-helper.md",
                "openspec-apply.md",
                "openspec-archive.md",
            ]
        );
    }

    // ---- codexGhostAdapter --------------------------------------------------

    #[test]
    fn codex_enumerates_prompts_skills_rules_memories() {
        let candidates = CodexGhostAdapter.enumerate(&make_inputs());
        let mut by_kind: HashMap<GhostFindingKind, Vec<String>> = HashMap::new();
        for c in &candidates {
            by_kind.entry(c.kind).or_default().push(c.basename.clone());
        }
        for v in by_kind.values_mut() {
            v.sort();
        }
        assert_eq!(
            by_kind
                .get(&GhostFindingKind::GhostPrompt)
                .cloned()
                .unwrap_or_default(),
            vec!["openspec-apply.md", "openspec-archive.md", "refactor.md"]
        );
        assert_eq!(
            by_kind
                .get(&GhostFindingKind::GhostSkill)
                .cloned()
                .unwrap_or_default(),
            vec!["code-search.md"]
        );
        assert_eq!(
            by_kind
                .get(&GhostFindingKind::GhostRule)
                .cloned()
                .unwrap_or_default(),
            vec!["no-print.md"]
        );
        assert_eq!(
            by_kind
                .get(&GhostFindingKind::GhostMemory)
                .cloned()
                .unwrap_or_default(),
            vec!["preferences.md"]
        );
    }

    #[test]
    fn codex_flags_openspec_archive_as_ghost() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &["refactor", "code-search"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 5)]);
        let ghosts = detect_ghost_surface(&inputs);
        let codex_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::Codex)
            .collect();
        let openspec = codex_ghosts
            .iter()
            .find(|g| g.path.ends_with("openspec-archive.md"));
        assert!(openspec.is_some());
        assert_eq!(openspec.unwrap().kind, GhostFindingKind::GhostPrompt);
        assert_eq!(openspec.unwrap().session_count, 5);
        assert!(openspec.unwrap().cost > 0.0);
        let kinds: HashSet<GhostFindingKind> = codex_ghosts.iter().map(|g| g.kind).collect();
        assert!(kinds.contains(&GhostFindingKind::GhostRule));
        assert!(kinds.contains(&GhostFindingKind::GhostMemory));
    }

    #[test]
    fn codex_de_ghosts_via_slash_in_user_text() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &["refactor", "code-search"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 5)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "session-1".to_string(),
                vec!["/openspec-apply\nApply the latest proposal please.".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let codex_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::Codex)
            .collect();
        let apply = codex_ghosts
            .iter()
            .find(|g| g.path.ends_with("openspec-apply.md"));
        assert!(apply.is_none(), "codex openspec-apply should be de-ghosted");
        let archive = codex_ghosts
            .iter()
            .find(|g| g.path.ends_with("openspec-archive.md"));
        assert!(
            archive.is_some(),
            "codex openspec-archive should remain a ghost"
        );
    }

    #[test]
    fn codex_recognises_slash_not_at_start() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &[]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "session-1".to_string(),
                vec!["Please run the /openspec-apply prompt now.".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(apply.is_none(), "mid-line /openspec-apply should de-ghost");
    }

    #[test]
    fn codex_does_not_match_extended_slash_command() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &[]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "session-1".to_string(),
                vec!["/openspec-apply-foo bar".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(
            apply.is_some(),
            "a longer slash command should not de-ghost the shorter stem"
        );
    }

    #[test]
    fn codex_ignores_slash_after_word_char() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &[]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "session-1".to_string(),
                vec!["See https://example.com/openspec-apply for docs.".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(
            apply.is_some(),
            "URL-style /openspec-apply should not de-ghost"
        );
    }

    #[test]
    fn codex_matches_case_insensitively() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Codex, &[]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "session-1".to_string(),
                vec!["/OPENSPEC-Apply now".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(
            apply.is_none(),
            "mixed-case /OPENSPEC-Apply should de-ghost"
        );
    }

    #[test]
    fn codex_does_not_de_ghost_from_claude_command_marker() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed_multi(&[
            (SourceKind::ClaudeCode, &["code-reviewer"]),
            (SourceKind::Codex, &["refactor"]),
        ]);
        inputs.session_count_by_source =
            count_map(&[(SourceKind::ClaudeCode, 1), (SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::ClaudeCode,
            vec![(
                "claude-session-1".to_string(),
                vec!["<command-name>/openspec-apply</command-name>\nbody".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let codex_apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(codex_apply.is_some(), "Codex must remain a ghost");
        let claude_apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::ClaudeCode && g.path.ends_with("openspec-apply.md"));
        assert!(
            claude_apply.is_none(),
            "Claude side is de-ghosted by its own marker"
        );
    }

    #[test]
    fn claude_does_not_de_ghost_from_codex_slash() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed_multi(&[
            (SourceKind::ClaudeCode, &["code-reviewer"]),
            (SourceKind::Codex, &["refactor"]),
        ]);
        inputs.session_count_by_source =
            count_map(&[(SourceKind::ClaudeCode, 1), (SourceKind::Codex, 1)]);
        inputs.user_turn_text_by_session = Some(user_text(vec![(
            SourceKind::Codex,
            vec![(
                "codex-session-1".to_string(),
                vec!["/openspec-apply\nApply the latest proposal.".to_string()],
            )],
        )]));
        let ghosts = detect_ghost_surface(&inputs);
        let claude_apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::ClaudeCode && g.path.ends_with("openspec-apply.md"));
        assert!(
            claude_apply.is_some(),
            "Claude must remain a ghost — Codex slash mustn't leak"
        );
        let codex_apply = ghosts
            .iter()
            .find(|g| g.source == SourceKind::Codex && g.path.ends_with("openspec-apply.md"));
        assert!(codex_apply.is_none());
    }

    // ---- opencodeGhostAdapter ----------------------------------------------

    #[test]
    fn opencode_enumerates_declared_skills_commands_and_project_skills() {
        let candidates = OpenCodeGhostAdapter.enumerate(&make_inputs());
        let declared: Vec<&GhostCandidate> = candidates
            .iter()
            .filter(|c| c.counted_by_catalog_bloat == Some(true))
            .collect();
        let project: Vec<&GhostCandidate> = candidates
            .iter()
            .filter(|c| c.counted_by_catalog_bloat != Some(true))
            .collect();
        let mut declared_names: Vec<String> = declared.iter().map(|c| c.basename.clone()).collect();
        declared_names.sort();
        assert_eq!(
            declared_names,
            vec!["abandoned-helper", "code-search"],
            "declared catalog skills are flagged with countedByCatalogBloat",
        );
        let project_skills: Vec<String> = project
            .iter()
            .filter(|c| c.kind == GhostFindingKind::GhostSkill)
            .map(|c| c.basename.clone())
            .collect();
        assert_eq!(project_skills, vec!["project-skill.md"]);
        let mut commands: Vec<String> = project
            .iter()
            .filter(|c| c.kind == GhostFindingKind::GhostCommand)
            .map(|c| c.basename.clone())
            .collect();
        commands.sort();
        assert_eq!(commands, vec!["deploy", "ghost-command"]);
    }

    #[test]
    fn opencode_emits_zero_cost_for_declared_catalog_bloat() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source =
            observed(SourceKind::Opencode, &["code-search", "deploy"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Opencode, 20)]);
        let ghosts = detect_ghost_surface(&inputs);
        let opencode_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::Opencode)
            .collect();
        let abandoned = opencode_ghosts
            .iter()
            .find(|g| g.path.contains("abandoned-helper"));
        assert!(abandoned.is_some(), "declared catalog skill is reported");
        assert_eq!(abandoned.unwrap().cost, 0.0);
        assert_eq!(abandoned.unwrap().counted_by_catalog_bloat, Some(true));
        let ghost_cmd = opencode_ghosts
            .iter()
            .find(|g| g.path.ends_with("#/commands/ghost-command"));
        assert!(ghost_cmd.is_some());
        assert!(ghost_cmd.unwrap().cost > 0.0);
        assert_eq!(ghost_cmd.unwrap().counted_by_catalog_bloat, None);
        let project_skill = opencode_ghosts
            .iter()
            .find(|g| g.path.ends_with("project-skill.md"));
        assert!(project_skill.is_some());
        assert!(project_skill.unwrap().cost > 0.0);
        assert_eq!(project_skill.unwrap().counted_by_catalog_bloat, None);
    }

    // ---- detectGhostSurface — orchestrator ---------------------------------

    #[test]
    fn orchestrator_runs_every_adapter_sorted_by_cost_desc() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed_multi(&[
            (SourceKind::ClaudeCode, &["code-reviewer", "git-commit"]),
            (SourceKind::Codex, &["refactor", "code-search"]),
            (SourceKind::Opencode, &["code-search", "deploy"]),
        ]);
        inputs.session_count_by_source = count_map(&[
            (SourceKind::ClaudeCode, 10),
            (SourceKind::Codex, 5),
            (SourceKind::Opencode, 20),
        ]);
        let ghosts = detect_ghost_surface(&inputs);
        for w in ghosts.windows(2) {
            assert!(w[0].cost >= w[1].cost, "sorted by cost desc");
        }
        let sources: HashSet<SourceKind> = ghosts.iter().map(|g| g.source).collect();
        assert!(sources.contains(&SourceKind::ClaudeCode));
        assert!(sources.contains(&SourceKind::Codex));
        assert!(sources.contains(&SourceKind::Opencode));
    }

    #[test]
    fn orchestrator_treats_observed_case_insensitively() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(
            SourceKind::ClaudeCode,
            &[
                "Code-Reviewer",
                "GIT-COMMIT",
                "forgotten-HELPER",
                "openspec-archive",
                "openspec-apply",
            ],
        );
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 1)]);
        let ghosts = detect_ghost_surface(&inputs);
        let claude_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::ClaudeCode)
            .collect();
        assert_eq!(claude_ghosts.len(), 0);
    }

    #[test]
    fn orchestrator_includes_ghost_when_session_count_zero() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::ClaudeCode, &[]);
        let ghosts = detect_ghost_surface(&inputs);
        let claude_ghosts: Vec<&GhostSurfaceFinding> = ghosts
            .iter()
            .filter(|g| g.source == SourceKind::ClaudeCode)
            .collect();
        assert!(!claude_ghosts.is_empty());
        for g in &claude_ghosts {
            assert_eq!(g.cost, 0.0);
            assert_eq!(g.session_count, 0);
        }
    }

    // ---- ghostSurfaceToFinding ---------------------------------------------

    #[test]
    fn finding_produces_mv_command_action() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::ClaudeCode, &["code-reviewer"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 10)]);
        let ghosts = detect_ghost_surface(&inputs);
        let helper = ghosts
            .iter()
            .find(|g| g.path.ends_with("forgotten-helper.md"))
            .unwrap();
        let finding = ghost_surface_to_finding(
            helper,
            &GhostSurfaceFindingOptions {
                archive_dir: Some(PathBuf::from("/tmp/ghost-archive")),
            },
        );
        assert_eq!(finding.kind, "ghost-agent");
        assert_eq!(finding.actions.len(), 1);
        match &finding.actions[0] {
            WasteAction::Command { text, .. } => {
                assert!(text.contains("mv "));
                assert!(text.contains("/tmp/ghost-archive"));
                assert!(text.contains(&helper.path));
            }
            other => panic!("expected Command, got {other:?}"),
        }
        assert!(finding.title.contains("forgotten-helper"));
        assert!(finding.detail.contains("claude-code"));
    }

    #[test]
    fn finding_marks_catalog_bloat_with_zero_cost_and_dedup_note() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Opencode, &["deploy"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Opencode, 100)]);
        let ghosts = detect_ghost_surface(&inputs);
        let abandoned = ghosts
            .iter()
            .find(|g| g.path.contains("abandoned-helper"))
            .unwrap();
        let finding = ghost_surface_to_finding(abandoned, &GhostSurfaceFindingOptions::default());
        assert_eq!(finding.estimated_savings.usd_per_session, Some(0.0));
        assert!(finding.detail.contains("catalog-bloat"));
    }

    #[test]
    fn finding_uses_per_session_cost_for_severity() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::ClaudeCode, &["code-reviewer"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::ClaudeCode, 100_000)]);
        let ghosts = detect_ghost_surface(&inputs);
        let helper = ghosts
            .iter()
            .find(|g| g.path.ends_with("forgotten-helper.md"))
            .unwrap();
        // Cumulative cost is well above $1 (severity High threshold = $0.5).
        assert!(helper.cost > 1.0, "expected cumulative cost > $1");
        // Per-session cost should be far below $0.05 (severity Warn threshold).
        assert!(
            helper.cost_per_session < 0.05,
            "per-session cost should be below warn threshold"
        );
        let finding = ghost_surface_to_finding(
            helper,
            &GhostSurfaceFindingOptions {
                archive_dir: Some(PathBuf::from("/tmp/ghost-archive")),
            },
        );
        assert_eq!(
            finding.estimated_savings.usd_per_session,
            Some(helper.cost_per_session)
        );
        assert_eq!(finding.severity, WasteSeverity::Info);
    }

    #[test]
    fn finding_shell_quotes_paths_with_spaces() {
        let ghost = GhostSurfaceFinding {
            source: SourceKind::ClaudeCode,
            kind: GhostFindingKind::GhostAgent,
            path: "/Users/me/.claude/agents/my helper.md".to_string(),
            size_tokens: 100,
            cost: 0.001,
            cost_per_session: 0.0001,
            session_count: 10,
            counted_by_catalog_bloat: None,
        };
        let finding = ghost_surface_to_finding(
            &ghost,
            &GhostSurfaceFindingOptions {
                archive_dir: Some(PathBuf::from("/tmp/ghost archive")),
            },
        );
        match &finding.actions[0] {
            WasteAction::Command { text, .. } => {
                assert!(text.contains("'/Users/me/.claude/agents/my helper.md'"));
                assert!(text.contains("'/tmp/ghost archive"));
            }
            other => panic!("expected Command action, got {other:?}"),
        }
    }

    #[test]
    fn finding_emits_paste_for_synthetic_opencode_paths() {
        let mut inputs = make_inputs();
        inputs.observed_names_by_source = observed(SourceKind::Opencode, &["deploy"]);
        inputs.session_count_by_source = count_map(&[(SourceKind::Opencode, 5)]);
        let ghosts = detect_ghost_surface(&inputs);
        let synthetic = ghosts
            .iter()
            .find(|g| g.path.contains("#/commands/ghost-command"))
            .unwrap();
        let finding = ghost_surface_to_finding(synthetic, &GhostSurfaceFindingOptions::default());
        match &finding.actions[0] {
            WasteAction::Paste { text, .. } => {
                assert!(!text.contains("mv "));
                assert!(text.contains("opencode.json"));
                assert!(text.contains("/commands/ghost-command"));
            }
            other => panic!("expected Paste action, got {other:?}"),
        }
    }
}
