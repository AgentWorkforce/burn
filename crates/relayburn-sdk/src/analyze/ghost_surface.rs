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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::reader::SourceKind;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::analyze::findings::{EstimatedSavings, WasteAction, WasteFinding};

mod adapters;
use adapters::default_ghost_adapters;
#[cfg(test)]
use adapters::{ClaudeGhostAdapter, CodexGhostAdapter, OpenCodeGhostAdapter};

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
        let source_str = ghost.source.wire_str();
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
    let source_str = ghost.source.wire_str();
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "ghost_surface_tests.rs"]
mod tests;
