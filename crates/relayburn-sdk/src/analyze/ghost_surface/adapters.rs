//! Per-harness `GhostSurfaceAdapter` implementations and the filesystem
//! enumeration helpers they exclusively use.
//!
//! Each adapter (Claude / Codex / OpenCode) walks its own user-installed
//! surface directories and emits `GhostCandidate`s. The directory walker
//! (`DirEntry` / `list_dir_files`), the per-surface filename predicates
//! (`is_markdown` / `is_plain_text_surface`), and the OpenCode
//! config/catalog reader (`enumerate_opencode_project`) live here because
//! only the adapter cluster uses them. The slash-command miners and the
//! orchestrator stay in the parent module and are reached via `super::`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::reader::SourceKind;
use crate::util::home_dir;

use super::{
    mine_claude_command_names, mine_codex_slash_invocations, GhostCandidate, GhostFindingKind,
    GhostSurfaceAdapter, GhostSurfaceInputs,
};
use crate::analyze::util::tokens_from_bytes;

#[derive(Debug, Clone)]
pub(super) struct DirEntry {
    pub(super) path: PathBuf,
    pub(super) basename: String,
    pub(super) size: u64,
}

/// Walk a directory non-recursively. Returns regular files matching
/// `predicate`; returns `[]` when the directory doesn't exist. Mirrors
/// `listDirFiles` in TS.
pub(super) fn list_dir_files<F>(dir: &Path, predicate: F) -> Vec<DirEntry>
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
                    size_tokens: tokens_from_bytes(file.size),
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
                    size_tokens: tokens_from_bytes(file.size),
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
                                tokens_from_bytes(serialized.len() as u64),
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
                size_tokens: tokens_from_bytes(file.size),
                counted_by_catalog_bloat: None,
            });
        }
    }

    out
}

/// Default adapter registry. Mirrors `DEFAULT_GHOST_ADAPTERS` in TS.
pub(super) fn default_ghost_adapters() -> Vec<Box<dyn GhostSurfaceAdapter>> {
    vec![
        Box::new(ClaudeGhostAdapter),
        Box::new(CodexGhostAdapter),
        Box::new(OpenCodeGhostAdapter),
    ]
}
