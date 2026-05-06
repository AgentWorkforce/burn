//! Pure aggregations that build the per-source inputs the ghost-surface
//! detector consumes. Rust port of `packages/analyze/src/ghost-surface-inputs.ts`
//! — see AgentWorkforce/burn#273.
//!
//! These functions take a slice of `TurnRecord` (the shape that lands in
//! the burn ledger). The TS counterpart takes `EnrichedTurn[]`, but none of
//! the read fields live on the stamp enrichment, so we operate on the
//! underlying `TurnRecord` directly to avoid pulling the ledger crate into
//! `relayburn-analyze`'s dependency surface.
//!
//! The optional `userTurnTextBySession` map (used by Claude /
//! Codex slash-command miners) is sourced from the ledger's content
//! sidecar in the production CLI; loading it lives in the CLI layer
//! rather than here so the analyze crate stays I/O-free.

use std::collections::{HashMap, HashSet};

use crate::reader::{SourceKind, TurnRecord};

use crate::analyze::ghost_surface::GhostSurfaceInputs;
use crate::analyze::pricing::PricingTable;

/// Per-source set of *observed invocation names* in this slice. Each turn
/// contributes its tool-call names, any `skillName` set on a tool call, and
/// the subagent type (when present).
pub fn build_observed_names_by_source(
    turns: &[TurnRecord],
) -> HashMap<SourceKind, HashSet<String>> {
    let mut out: HashMap<SourceKind, HashSet<String>> = HashMap::new();
    for t in turns {
        let entry = out.entry(t.source).or_default();
        for call in &t.tool_calls {
            entry.insert(call.name.clone());
            if let Some(skill) = &call.skill_name {
                entry.insert(skill.clone());
            }
        }
        if let Some(sub) = &t.subagent {
            if let Some(stype) = &sub.subagent_type {
                entry.insert(stype.clone());
            }
        }
    }
    out
}

/// Per-source count of distinct sessionIds seen in this slice. Drives the
/// cost multiplier (a ghost file rides in every one of those sessions).
pub fn build_session_count_by_source(turns: &[TurnRecord]) -> HashMap<SourceKind, u64> {
    let mut seen: HashMap<SourceKind, HashSet<String>> = HashMap::new();
    for t in turns {
        seen.entry(t.source)
            .or_default()
            .insert(t.session_id.clone());
    }
    seen.into_iter()
        .map(|(s, set)| (s, set.len() as u64))
        .collect()
}

/// Pick a representative dollar-per-token rate for ghost-surface costing.
/// User-installed surface rides in the CACHED prefix on every call after
/// the first, so the cacheRead rate is the right basis. Pricing values
/// in `PricingTable` are per million tokens, hence the `/ 1e6` conversion.
/// Falls back to 0 (which produces $0 cost but still surfaces ghosts) when
/// no priced model is available.
pub fn pick_representative_cache_read_rate(turns: &[TurnRecord], pricing: &PricingTable) -> f64 {
    let mut counts: HashMap<&str, u64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for t in turns {
        let m = t.model.as_str();
        if !counts.contains_key(m) {
            order.push(m);
        }
        *counts.entry(m).or_insert(0) += 1;
    }
    let mut best_model: Option<&str> = None;
    let mut best_count: i64 = -1;
    // Iterate in first-seen order so ties go to the earliest-seen model
    // (matches the TS `Map` iteration order).
    for m in order {
        let c = counts[m] as i64;
        if c > best_count && pricing.contains_key(m) {
            best_model = Some(m);
            best_count = c;
        }
    }
    match best_model {
        Some(m) => pricing[m].cache_read / 1_000_000.0,
        None => 0.0,
    }
}

/// Assemble a `GhostSurfaceInputs` from a slice of turns plus a pricing
/// table. The optional `user_turn_text_by_session` map should be loaded by
/// the caller (typically the CLI) from the content sidecar; pass `None`
/// to fall back to v1 (tool-call only) behaviour.
pub fn build_ghost_surface_inputs(
    turns: &[TurnRecord],
    pricing: &PricingTable,
    user_turn_text_by_session: Option<HashMap<SourceKind, HashMap<String, Vec<String>>>>,
) -> GhostSurfaceInputs {
    let observed = build_observed_names_by_source(turns);
    let counts = build_session_count_by_source(turns);
    let dollar_per_token = pick_representative_cache_read_rate(turns, pricing);
    let user_turn_text_by_session = user_turn_text_by_session.filter(|m| !m.is_empty());
    GhostSurfaceInputs {
        observed_names_by_source: observed,
        session_count_by_source: counts,
        dollar_per_token,
        claude_home: None,
        codex_home: None,
        opencode_projects: None,
        user_turn_text_by_session,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{Subagent, ToolCall, Usage};

    use crate::analyze::pricing::{ModelCost, ReasoningMode};

    fn make_turn(
        source: SourceKind,
        session_id: &str,
        model: &str,
        tool_calls: Vec<ToolCall>,
        subagent: Option<Subagent>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            session_path: None,
            message_id: format!("msg-{session_id}"),
            turn_index: 0,
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            model: model.to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 0,
                output: 0,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls,
            files_touched: None,
            subagent,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn tool_call(name: &str, skill: Option<&str>) -> ToolCall {
        ToolCall {
            id: "id".to_string(),
            name: name.to_string(),
            target: None,
            args_hash: "h".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: skill.map(|s| s.to_string()),
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn subagent(stype: &str) -> Subagent {
        Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: None,
            parent_agent_id: None,
            subagent_type: Some(stype.to_string()),
            description: None,
        }
    }

    #[test]
    fn observed_names_unions_calls_skills_and_subagents() {
        let turns = vec![
            make_turn(
                SourceKind::ClaudeCode,
                "s1",
                "claude-sonnet-4-6",
                vec![tool_call("Read", None), tool_call("Skill", Some("init"))],
                Some(subagent("code-reviewer")),
            ),
            make_turn(
                SourceKind::Codex,
                "s2",
                "gpt",
                vec![tool_call("read_file", None)],
                None,
            ),
        ];
        let observed = build_observed_names_by_source(&turns);
        let claude = observed.get(&SourceKind::ClaudeCode).unwrap();
        assert!(claude.contains("Read"));
        assert!(claude.contains("Skill"));
        assert!(claude.contains("init"));
        assert!(claude.contains("code-reviewer"));
        let codex = observed.get(&SourceKind::Codex).unwrap();
        assert!(codex.contains("read_file"));
    }

    #[test]
    fn session_count_dedups_by_session_id() {
        let turns = vec![
            make_turn(SourceKind::ClaudeCode, "s1", "m", vec![], None),
            make_turn(SourceKind::ClaudeCode, "s1", "m", vec![], None),
            make_turn(SourceKind::ClaudeCode, "s2", "m", vec![], None),
            make_turn(SourceKind::Codex, "s3", "m", vec![], None),
        ];
        let counts = build_session_count_by_source(&turns);
        assert_eq!(counts.get(&SourceKind::ClaudeCode).copied(), Some(2));
        assert_eq!(counts.get(&SourceKind::Codex).copied(), Some(1));
    }

    #[test]
    fn picks_cache_read_rate_of_most_used_priced_model() {
        let mut pricing: PricingTable = HashMap::new();
        pricing.insert(
            "claude-sonnet-4-6".to_string(),
            ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.3,
                cache_write: 0.0,
                reasoning: None,
                reasoning_mode: ReasoningMode::IncludedInOutput,
            },
        );
        pricing.insert(
            "claude-opus-4-7".to_string(),
            ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 1.5,
                cache_write: 0.0,
                reasoning: None,
                reasoning_mode: ReasoningMode::IncludedInOutput,
            },
        );
        let turns = vec![
            make_turn(
                SourceKind::ClaudeCode,
                "s1",
                "claude-sonnet-4-6",
                vec![],
                None,
            ),
            make_turn(
                SourceKind::ClaudeCode,
                "s1",
                "claude-sonnet-4-6",
                vec![],
                None,
            ),
            make_turn(
                SourceKind::ClaudeCode,
                "s2",
                "claude-opus-4-7",
                vec![],
                None,
            ),
        ];
        let rate = pick_representative_cache_read_rate(&turns, &pricing);
        assert!((rate - 0.3 / 1_000_000.0).abs() < 1e-15);
    }

    #[test]
    fn picks_zero_when_no_priced_model() {
        let pricing: PricingTable = HashMap::new();
        let turns = vec![make_turn(SourceKind::Codex, "s1", "unknown", vec![], None)];
        assert_eq!(pick_representative_cache_read_rate(&turns, &pricing), 0.0);
    }

    #[test]
    fn build_inputs_drops_empty_user_turn_text() {
        let pricing: PricingTable = HashMap::new();
        let inputs = build_ghost_surface_inputs(&[], &pricing, Some(HashMap::new()));
        assert!(inputs.user_turn_text_by_session.is_none());
    }
}
