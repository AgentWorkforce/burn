//! Conformance tests for the ghost_surface module — extracted verbatim from the
//! former inline `#[cfg(test)] mod tests` block (included via `#[path]`).

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
    let mut basenames: Vec<String> = claude_ghosts.iter().map(|g| basename_of(&g.path)).collect();
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
    inputs.observed_names_by_source = observed(SourceKind::Opencode, &["code-search", "deploy"]);
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
