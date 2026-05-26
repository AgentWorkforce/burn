//! Tool-call-pattern detector — Rust port of
//! `packages/analyze/src/tool-call-patterns.ts`.
//!
//! Surfaces vanilla tool-call sequences that materialize a lot of intermediate
//! output an agent could have collapsed by consolidating calls or reaching
//! for a higher-level tool. Vendor-neutral: emits the pattern + the tokens-of-
//! overhead estimate. Reads only `TurnRecord.tool_calls` so it runs on any
//! slice with `has_tool_calls` coverage.

use std::collections::{BTreeMap, HashSet};

use crate::reader::{
    normalize_tool_name, parse_bash_command, BashParse, SourceKind, ToolCall, TurnRecord,
};
use phf::phf_set;
use serde::{Deserialize, Serialize};

use crate::analyze::cost::lookup_model_rate;
use crate::analyze::findings::{severity_from_usd, EstimatedSavings, WasteAction, WasteFinding};
use crate::analyze::pricing::PricingTable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCallPatternCategory {
    SearchSequence,
    EditCluster,
    BashGitState,
    BashTestRun,
    BashGhPr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallPatternFinding {
    pub source: SourceKind,
    pub session_id: String,
    pub category: ToolCallPatternCategory,
    /// Number of vanilla calls (or sequences, for `SearchSequence`).
    pub occurrence_count: u64,
    /// Estimated tokens of overhead the pattern materialized.
    pub estimated_tokens_saved: u64,
    /// USD overhead, priced at the session's dominant model's input rate.
    pub estimated_usd_saved: f64,
    /// First few turn indexes where the pattern fired (capped at 5).
    pub sample_turn_indexes: Vec<u64>,
    /// Free-form evidence — file paths for `EditCluster`, distinct bash verbs
    /// for the bash-* categories, empty for `SearchSequence`.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DetectToolCallPatternsOptions<'a> {
    pub pricing: &'a PricingTable,
}

// Per-occurrence token-overhead estimates. Mirror the TS constants verbatim.
const SAVINGS_PER_SEARCH_SEQUENCE: u64 = 2500;
const SAVINGS_PER_EXTRA_EDIT_IN_CLUSTER: u64 = 400;
const SAVINGS_PER_GIT_STATE_CALL: u64 = 800;
const SAVINGS_PER_TEST_RUN_CALL: u64 = 1200;
const SAVINGS_PER_GH_PR_CALL: u64 = 600;

const SEARCH_SEQUENCE_MIN_PER_SESSION: usize = 3;
const EDIT_CLUSTER_MIN: usize = 3;
const EDIT_CLUSTER_TURN_WINDOW: u64 = 5;

static BASH_RAW_NAMES: phf::Set<&'static str> = phf_set! {
    "Bash", "bash", "exec_command", "shell"
};

pub fn detect_tool_call_patterns(
    turns: &[TurnRecord],
    opts: &DetectToolCallPatternsOptions<'_>,
) -> Vec<ToolCallPatternFinding> {
    // Bucket by session preserving first-seen order to match TS `Map`.
    let mut order: Vec<String> = Vec::new();
    let mut by_session: std::collections::HashMap<String, Vec<&TurnRecord>> =
        std::collections::HashMap::new();
    for t in turns {
        by_session
            .entry(t.session_id.clone())
            .or_insert_with(|| {
                order.push(t.session_id.clone());
                Vec::new()
            })
            .push(t);
    }
    let mut out: Vec<ToolCallPatternFinding> = Vec::new();
    for sid in &order {
        let mut sess = by_session.remove(sid).unwrap();
        sess.sort_by_key(|t| t.turn_index);
        out.extend(detect_for_session(sid, &sess, opts.pricing));
    }
    // Sort: usd desc, then tokens desc.
    out.sort_by(|a, b| {
        b.estimated_usd_saved
            .partial_cmp(&a.estimated_usd_saved)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.estimated_tokens_saved.cmp(&a.estimated_tokens_saved))
    });
    out
}

fn detect_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<ToolCallPatternFinding> {
    if turns.is_empty() {
        return Vec::new();
    }
    let source = turns[0].source;
    let input_rate = pick_input_rate(turns, pricing);
    let mut out: Vec<ToolCallPatternFinding> = Vec::new();

    // Search sequences.
    let mut search_turns: Vec<u64> = Vec::new();
    for t in turns {
        if turn_has_search_sequence(&t.tool_calls) {
            search_turns.push(t.turn_index);
        }
    }
    if search_turns.len() >= SEARCH_SEQUENCE_MIN_PER_SESSION {
        let count = search_turns.len() as u64;
        let tokens = count * SAVINGS_PER_SEARCH_SEQUENCE;
        out.push(ToolCallPatternFinding {
            source,
            session_id: session_id.to_string(),
            category: ToolCallPatternCategory::SearchSequence,
            occurrence_count: count,
            estimated_tokens_saved: tokens,
            estimated_usd_saved: price_tokens(tokens, input_rate),
            sample_turn_indexes: search_turns.iter().take(5).copied().collect(),
            evidence: Vec::new(),
        });
    }

    // Edit clusters.
    for cluster in detect_edit_clusters(turns) {
        let extras = cluster.edit_count.saturating_sub(1);
        let tokens = extras * SAVINGS_PER_EXTRA_EDIT_IN_CLUSTER;
        out.push(ToolCallPatternFinding {
            source,
            session_id: session_id.to_string(),
            category: ToolCallPatternCategory::EditCluster,
            occurrence_count: cluster.edit_count,
            estimated_tokens_saved: tokens,
            estimated_usd_saved: price_tokens(tokens, input_rate),
            sample_turn_indexes: cluster.turn_indexes.iter().take(5).copied().collect(),
            evidence: vec![cluster.file_path],
        });
    }

    // Bash sub-verb matches.
    let mut git_state: Vec<BashHit> = Vec::new();
    let mut test_run: Vec<BashHit> = Vec::new();
    let mut gh_pr: Vec<BashHit> = Vec::new();
    for t in turns {
        for call in &t.tool_calls {
            if !BASH_RAW_NAMES.contains(call.name.as_str()) {
                continue;
            }
            let Some(target) = call.target.as_deref() else {
                continue;
            };
            let Some(parsed) = parse_bash_command(target) else {
                continue;
            };
            if matches_git_state(&parsed) {
                git_state.push(BashHit {
                    verb: parsed.normalized.clone(),
                    turn_index: t.turn_index,
                });
            } else if matches_test_run(&parsed) {
                test_run.push(BashHit {
                    verb: parsed.normalized.clone(),
                    turn_index: t.turn_index,
                });
            } else if matches_gh_pr(&parsed) {
                gh_pr.push(BashHit {
                    verb: parsed.normalized.clone(),
                    turn_index: t.turn_index,
                });
            }
        }
    }
    if !git_state.is_empty() {
        out.push(build_bash_finding(
            source,
            session_id,
            ToolCallPatternCategory::BashGitState,
            &git_state,
            SAVINGS_PER_GIT_STATE_CALL,
            input_rate,
        ));
    }
    if !test_run.is_empty() {
        out.push(build_bash_finding(
            source,
            session_id,
            ToolCallPatternCategory::BashTestRun,
            &test_run,
            SAVINGS_PER_TEST_RUN_CALL,
            input_rate,
        ));
    }
    if !gh_pr.is_empty() {
        out.push(build_bash_finding(
            source,
            session_id,
            ToolCallPatternCategory::BashGhPr,
            &gh_pr,
            SAVINGS_PER_GH_PR_CALL,
            input_rate,
        ));
    }

    out
}

#[derive(Debug, Clone)]
struct BashHit {
    verb: String,
    turn_index: u64,
}

fn build_bash_finding(
    source: SourceKind,
    session_id: &str,
    category: ToolCallPatternCategory,
    hits: &[BashHit],
    savings_per_call: u64,
    input_rate: f64,
) -> ToolCallPatternFinding {
    let count = hits.len() as u64;
    let tokens = count * savings_per_call;
    let turn_indexes: Vec<u64> = hits.iter().map(|h| h.turn_index).collect();
    let verbs: Vec<String> = hits.iter().map(|h| h.verb.clone()).collect();
    ToolCallPatternFinding {
        source,
        session_id: session_id.to_string(),
        category,
        occurrence_count: count,
        estimated_tokens_saved: tokens,
        estimated_usd_saved: price_tokens(tokens, input_rate),
        sample_turn_indexes: dedup_numbers(&turn_indexes).into_iter().take(5).collect(),
        evidence: dedup_strings(&verbs),
    }
}

/// True iff the turn's tool calls contain Glob → Grep → Read in order.
fn turn_has_search_sequence(calls: &[ToolCall]) -> bool {
    #[derive(Copy, Clone, PartialEq)]
    enum Stage {
        Glob,
        Grep,
        Read,
    }
    let mut stage = Stage::Glob;
    for call in calls {
        let name = normalize_tool_name(&call.name);
        match (stage, name) {
            (Stage::Glob, "Glob") => stage = Stage::Grep,
            (Stage::Grep, "Grep") => stage = Stage::Read,
            (Stage::Read, "Read") => return true,
            _ => {}
        }
    }
    false
}

#[derive(Debug)]
struct EditCluster {
    file_path: String,
    edit_count: u64,
    turn_indexes: Vec<u64>,
}

fn detect_edit_clusters(turns: &[&TurnRecord]) -> Vec<EditCluster> {
    // BTreeMap to keep stable cross-platform output ordering — file paths
    // sorted lexicographically. The TS Map preserves insertion order, but the
    // tests sort by file path before asserting so either ordering works; we
    // prefer determinism.
    let mut by_file: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for t in turns {
        for call in &t.tool_calls {
            let name = normalize_tool_name(&call.name);
            if name != "Edit" && name != "Write" && name != "NotebookEdit" {
                continue;
            }
            let Some(target) = &call.target else { continue };
            by_file
                .entry(target.clone())
                .or_default()
                .push(t.turn_index);
        }
    }
    let mut out: Vec<EditCluster> = Vec::new();
    for (file_path, mut turn_indexes) in by_file {
        if turn_indexes.len() < EDIT_CLUSTER_MIN {
            continue;
        }
        turn_indexes.sort_unstable();
        let mut best_count: usize = 0;
        let mut best_window: Vec<u64> = Vec::new();
        for i in 0..turn_indexes.len() {
            let start = turn_indexes[i];
            let mut window: Vec<u64> = Vec::new();
            for &v in &turn_indexes[i..] {
                if v - start >= EDIT_CLUSTER_TURN_WINDOW {
                    break;
                }
                window.push(v);
            }
            if window.len() > best_count {
                best_count = window.len();
                best_window = window;
            }
        }
        if best_count >= EDIT_CLUSTER_MIN {
            out.push(EditCluster {
                file_path,
                edit_count: best_count as u64,
                turn_indexes: best_window,
            });
        }
    }
    out
}

fn matches_git_state(parsed: &BashParse) -> bool {
    if parsed.binary != "git" {
        return false;
    }
    matches!(
        parsed.subcommand.as_deref(),
        Some("status" | "diff" | "log")
    )
}

fn matches_test_run(parsed: &BashParse) -> bool {
    let bin = parsed.binary.as_str();
    if matches!(bin, "pytest" | "jest" | "vitest") {
        return true;
    }
    if bin == "cargo" && parsed.subcommand.as_deref() == Some("test") {
        return true;
    }
    if bin == "go" && parsed.subcommand.as_deref() == Some("test") {
        return true;
    }
    if matches!(bin, "pnpm" | "npm" | "yarn" | "bun") {
        if let Some(sub) = parsed.subcommand.as_deref() {
            return sub == "test" || sub.starts_with("test:");
        }
    }
    false
}

fn matches_gh_pr(parsed: &BashParse) -> bool {
    if parsed.binary != "gh" {
        return false;
    }
    let Some(sub) = parsed.subcommand.as_deref() else {
        return false;
    };
    sub == "api" || sub == "pr" || sub.starts_with("pr ")
}

/// Pick the dominant model's input rate (USD per token) for the session.
/// Ties go to the first-seen model. Returns 0 when no priced model is
/// available — the finding still emits, just with $0.
fn pick_input_rate(turns: &[&TurnRecord], pricing: &PricingTable) -> f64 {
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for t in turns {
        let entry = counts.entry(t.model.clone()).or_insert_with(|| {
            order.push(t.model.clone());
            0
        });
        *entry += 1;
    }
    let mut best: Option<(&str, u64)> = None;
    for model in &order {
        let count = counts[model];
        match best {
            Some((_, c)) if c >= count => {}
            _ => best = Some((model.as_str(), count)),
        }
    }
    let Some((best_model, _)) = best else {
        return 0.0;
    };
    match lookup_model_rate(best_model, pricing) {
        Some(rate) => rate.input / 1_000_000.0,
        None => 0.0,
    }
}

fn price_tokens(tokens: u64, rate_per_token: f64) -> f64 {
    if rate_per_token == 0.0 || tokens == 0 {
        return 0.0;
    }
    tokens as f64 * rate_per_token
}

fn dedup_numbers(xs: &[u64]) -> Vec<u64> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut out: Vec<u64> = Vec::new();
    for &x in xs {
        if seen.insert(x) {
            out.push(x);
        }
    }
    out.sort_unstable();
    out
}

fn dedup_strings(xs: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for x in xs {
        if seen.insert(x.clone()) {
            out.push(x.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WasteFinding adapter
// ---------------------------------------------------------------------------

fn fmt_usd(n: f64) -> String {
    format!("${:.4}", n)
}

fn category_title(c: ToolCallPatternCategory) -> &'static str {
    match c {
        ToolCallPatternCategory::SearchSequence => "Glob → Grep → Read sequence",
        ToolCallPatternCategory::EditCluster => "Edit cluster on a single file",
        ToolCallPatternCategory::BashGitState => "Vanilla git state via Bash",
        ToolCallPatternCategory::BashTestRun => "Vanilla test run via Bash",
        ToolCallPatternCategory::BashGhPr => "Vanilla gh pr / gh api via Bash",
    }
}

fn category_reason(c: ToolCallPatternCategory) -> &'static str {
    match c {
        ToolCallPatternCategory::SearchSequence =>
            "Discovery + filtering + reading three separate tools in one turn lands a lot of intermediate \
output in context. A consolidated discovery tool would collapse the round-trip into a single \
condensed result.",
        ToolCallPatternCategory::EditCluster =>
            "A burst of single edits on one file echoes the surrounding context on every call. \
A batched edit tool would fold N point edits into one round-trip.",
        ToolCallPatternCategory::BashGitState =>
            "git status / diff / log dump unbounded raw text. A structured-summary replacement would \
return only the bytes the agent actually uses.",
        ToolCallPatternCategory::BashTestRun =>
            "Test runners dump full per-suite output. A structured-summary replacement would return \
just pass/fail counts plus the first failure detail.",
        ToolCallPatternCategory::BashGhPr =>
            "gh pr view / gh api return raw JSON blobs. A structured-summary replacement would return \
only the PR fields the agent reads.",
    }
}

fn hotspots_action(session_id: &str) -> WasteAction {
    WasteAction::Command {
        label: "Inspect this session".to_string(),
        text: format!("burn hotspots --session {session_id}"),
    }
}

use super::util::format_with_commas;

pub fn tool_call_pattern_to_finding(finding: &ToolCallPatternFinding) -> WasteFinding {
    let evidence_str = if finding.evidence.is_empty() {
        String::new()
    } else {
        let head: Vec<&str> = finding
            .evidence
            .iter()
            .take(3)
            .map(|s| s.as_str())
            .collect();
        let extra = finding.evidence.len().saturating_sub(3);
        let tail = if extra > 0 {
            format!(", +{extra} more")
        } else {
            String::new()
        };
        format!(" Evidence: {}{}.", head.join(", "), tail)
    };
    let source_str = match serde_json::to_value(finding.source) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    };
    WasteFinding {
        kind: "tool-call-pattern".to_string(),
        severity: severity_from_usd(finding.estimated_usd_saved),
        session_id: finding.session_id.clone(),
        title: format!(
            "{}: {}×",
            category_title(finding.category),
            finding.occurrence_count
        ),
        detail: format!(
            "{reason} Observed {n} occurrence(s) in this {source} session. \
Estimated overhead: {tokens} tokens ({usd} at this session's input rate).{evidence}",
            reason = category_reason(finding.category),
            n = finding.occurrence_count,
            source = source_str,
            tokens = format_with_commas(finding.estimated_tokens_saved),
            usd = fmt_usd(finding.estimated_usd_saved),
            evidence = evidence_str,
        ),
        estimated_savings: EstimatedSavings {
            tokens_per_session: Some(finding.estimated_tokens_saved),
            usd_per_session: Some(finding.estimated_usd_saved),
            ..Default::default()
        },
        actions: vec![hotspots_action(&finding.session_id)],
        event_source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::findings::WasteSeverity;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{SourceKind, Usage};

    fn usage() -> Usage {
        Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn tc(id: &str, name: &str, target: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            target: target.map(String::from),
            args_hash: format!("{name}:{}", target.unwrap_or(id)),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn turn(
        session_id: &str,
        message_id: &str,
        turn_index: u64,
        source: SourceKind,
        tool_calls: Vec<ToolCall>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            session_path: None,
            message_id: message_id.to_string(),
            turn_index,
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: usage(),
            tool_calls,
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn pricing() -> PricingTable {
        load_builtin_pricing()
    }

    #[test]
    fn flags_three_or_more_search_sequences_in_a_session() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..4 {
            turns.push(turn(
                "s1",
                &format!("m{i}"),
                i,
                SourceKind::ClaudeCode,
                vec![
                    tc(&format!("g{i}"), "Glob", Some("*.ts")),
                    tc(&format!("r{i}"), "Grep", Some("foo")),
                    tc(&format!("d{i}"), "Read", Some(&format!("/path/{i}.ts"))),
                ],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let search = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::SearchSequence)
            .expect("search-sequence");
        assert_eq!(search.occurrence_count, 4);
        assert!(search.estimated_tokens_saved > 0);
        assert!(search.estimated_usd_saved > 0.0);
    }

    #[test]
    fn does_not_flag_search_below_threshold() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![
                    tc("a", "Glob", Some("*.ts")),
                    tc("b", "Grep", Some("foo")),
                    tc("c", "Read", Some("/x.ts")),
                ],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![
                    tc("d", "Glob", Some("*.ts")),
                    tc("e", "Grep", Some("bar")),
                    tc("f", "Read", Some("/y.ts")),
                ],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        assert!(out
            .iter()
            .all(|f| f.category != ToolCallPatternCategory::SearchSequence));
    }

    #[test]
    fn search_sequence_respects_ordering() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..4 {
            turns.push(turn(
                "s",
                &format!("m{i}"),
                i,
                SourceKind::ClaudeCode,
                vec![
                    tc(&format!("r{i}"), "Read", Some(&format!("/x{i}.ts"))),
                    tc(&format!("g{i}"), "Grep", Some("foo")),
                    tc(&format!("b{i}"), "Glob", Some("*.ts")),
                ],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        assert!(out
            .iter()
            .all(|f| f.category != ToolCallPatternCategory::SearchSequence));
    }

    #[test]
    fn flags_three_or_more_edits_to_same_file_in_window() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..4 {
            turns.push(turn(
                "s",
                &format!("m{i}"),
                i,
                SourceKind::ClaudeCode,
                vec![tc(&format!("e{i}"), "Edit", Some("/src/foo.ts"))],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let cluster = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::EditCluster)
            .expect("edit-cluster");
        assert_eq!(cluster.occurrence_count, 4);
        assert_eq!(cluster.evidence, vec!["/src/foo.ts".to_string()]);
    }

    #[test]
    fn does_not_flag_edits_outside_5_turn_window() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("e0", "Edit", Some("/f.ts"))],
            ),
            turn(
                "s",
                "m10",
                10,
                SourceKind::ClaudeCode,
                vec![tc("e1", "Edit", Some("/f.ts"))],
            ),
            turn(
                "s",
                "m20",
                20,
                SourceKind::ClaudeCode,
                vec![tc("e2", "Edit", Some("/f.ts"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        assert!(out
            .iter()
            .all(|f| f.category != ToolCallPatternCategory::EditCluster));
    }

    #[test]
    fn caps_window_at_exactly_5_consecutive_turn_indexes() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("e0", "Edit", Some("/f.ts"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc("e1", "Edit", Some("/f.ts"))],
            ),
            turn(
                "s",
                "m4",
                4,
                SourceKind::ClaudeCode,
                vec![tc("e4", "Edit", Some("/f.ts"))],
            ),
            turn(
                "s",
                "m5",
                5,
                SourceKind::ClaudeCode,
                vec![tc("e5", "Edit", Some("/f.ts"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let cluster = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::EditCluster)
            .expect("edit-cluster");
        assert_eq!(cluster.occurrence_count, 3);
    }

    #[test]
    fn treats_each_file_independently() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..4 {
            turns.push(turn(
                "s",
                &format!("a{i}"),
                i,
                SourceKind::ClaudeCode,
                vec![tc(&format!("a{i}"), "Edit", Some("/a.ts"))],
            ));
            turns.push(turn(
                "s",
                &format!("b{i}"),
                i + 10,
                SourceKind::ClaudeCode,
                vec![tc(&format!("b{i}"), "Edit", Some("/b.ts"))],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let clusters: Vec<_> = out
            .iter()
            .filter(|f| f.category == ToolCallPatternCategory::EditCluster)
            .collect();
        assert_eq!(clusters.len(), 2);
        let mut files: Vec<&str> = clusters.iter().map(|c| c.evidence[0].as_str()).collect();
        files.sort();
        assert_eq!(files, vec!["/a.ts", "/b.ts"]);
    }

    #[test]
    fn flags_git_state_calls() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("a", "Bash", Some("git status"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc("b", "Bash", Some("git diff HEAD~1"))],
            ),
            turn(
                "s",
                "m2",
                2,
                SourceKind::ClaudeCode,
                vec![tc("c", "Bash", Some("git log --oneline -n 5"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let git = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::BashGitState)
            .expect("bash-git-state");
        assert_eq!(git.occurrence_count, 3);
        assert!(git.evidence.iter().any(|v| v == "git status"));
        assert!(git.evidence.iter().any(|v| v == "git diff"));
        assert!(git.evidence.iter().any(|v| v == "git log"));
    }

    #[test]
    fn flags_test_run_calls() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("a", "Bash", Some("pnpm test"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc("b", "Bash", Some("pytest -k foo"))],
            ),
            turn(
                "s",
                "m2",
                2,
                SourceKind::ClaudeCode,
                vec![tc("c", "Bash", Some("jest --watch"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let test = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::BashTestRun)
            .expect("bash-test-run");
        assert_eq!(test.occurrence_count, 3);
    }

    #[test]
    fn flags_gh_pr_and_gh_api_calls() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("a", "Bash", Some("gh pr view 123"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc(
                    "b",
                    "Bash",
                    Some("gh api repos/foo/bar/pulls/1/comments"),
                )],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let gh = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::BashGhPr)
            .expect("bash-gh-pr");
        assert_eq!(gh.occurrence_count, 2);
    }

    #[test]
    fn does_not_match_gh_project_or_gh_prerelease() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("a", "Bash", Some("gh project list"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc("b", "Bash", Some("gh project view 5"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        assert!(out
            .iter()
            .all(|f| f.category != ToolCallPatternCategory::BashGhPr));
    }

    #[test]
    fn does_not_match_unrelated_bash_commands() {
        let pricing = pricing();
        let turns = vec![
            turn(
                "s",
                "m0",
                0,
                SourceKind::ClaudeCode,
                vec![tc("a", "Bash", Some("ls -la"))],
            ),
            turn(
                "s",
                "m1",
                1,
                SourceKind::ClaudeCode,
                vec![tc("b", "Bash", Some("cat README.md"))],
            ),
        ];
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        assert!(out.is_empty());
    }

    #[test]
    fn normalizes_opencode_lowercase_tool_names_for_search_sequence() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            turns.push(turn(
                "s",
                &format!("m{i}"),
                i,
                SourceKind::Opencode,
                vec![
                    tc(&format!("g{i}"), "glob", Some("*.ts")),
                    tc(&format!("r{i}"), "grep", Some("foo")),
                    tc(&format!("d{i}"), "read", Some(&format!("/x{i}.ts"))),
                ],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let search = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::SearchSequence)
            .expect("search-sequence");
        assert_eq!(search.source, SourceKind::Opencode);
    }

    #[test]
    fn matches_codex_apply_patch_as_edit_for_clustering() {
        let pricing = pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..4 {
            turns.push(turn(
                "s",
                &format!("m{i}"),
                i,
                SourceKind::Codex,
                vec![tc(&format!("e{i}"), "apply_patch", Some("/src/x.ts"))],
            ));
        }
        let out =
            detect_tool_call_patterns(&turns, &DetectToolCallPatternsOptions { pricing: &pricing });
        let cluster = out
            .iter()
            .find(|f| f.category == ToolCallPatternCategory::EditCluster)
            .expect("edit-cluster");
        assert_eq!(cluster.source, SourceKind::Codex);
    }

    #[test]
    fn emits_vendor_neutral_waste_finding() {
        let finding = ToolCallPatternFinding {
            source: SourceKind::ClaudeCode,
            session_id: "s-abcd1234".to_string(),
            category: ToolCallPatternCategory::SearchSequence,
            occurrence_count: 5,
            estimated_tokens_saved: 12500,
            estimated_usd_saved: 0.075,
            sample_turn_indexes: vec![0, 1, 2, 3, 4],
            evidence: Vec::new(),
        };
        let f = tool_call_pattern_to_finding(&finding);
        assert_eq!(f.kind, "tool-call-pattern");
        assert_eq!(f.session_id, "s-abcd1234");
        assert_eq!(f.severity, WasteSeverity::Warn);
        assert!(f.title.contains("Glob → Grep → Read"), "title: {}", f.title);
        assert_eq!(f.estimated_savings.tokens_per_session, Some(12500));
        let usd = f.estimated_savings.usd_per_session.unwrap_or(0.0);
        assert!((usd - 0.075).abs() < 1e-9);
        assert_eq!(f.actions.len(), 1);
        match &f.actions[0] {
            WasteAction::Command { label, .. } => {
                assert!(label.contains("Inspect this session"), "label: {label}");
            }
            other => panic!("expected Command action, got {other:?}"),
        }
        assert!(!f.title.to_lowercase().contains("relaywash"));
        assert!(!f.detail.to_lowercase().contains("relaywash"));
    }

    #[test]
    fn category_serializes_to_kebab_case() {
        let s = serde_json::to_string(&ToolCallPatternCategory::SearchSequence).unwrap();
        assert_eq!(s, "\"search-sequence\"");
        let s = serde_json::to_string(&ToolCallPatternCategory::BashGhPr).unwrap();
        assert_eq!(s, "\"bash-gh-pr\"");
    }
}
