//! Behavioral pattern detection — Rust port of
//! `packages/analyze/src/patterns.ts`.
//!
//! Detects per-session patterns across an ordered turn stream: retry loops,
//! consecutive failure runs, cancellation runs, compaction losses,
//! edit-revert cycles, OpenCode skill recall duplicates, OpenCode skill
//! pruning protection, OpenCode system prompt tax, and edit-heavy sessions.
//! Each detector is a small state machine that consumes the turn stream
//! (optionally enriched with `ToolResultEventRecord` chronology and a
//! content sidecar) and returns its narrow result type.
//!
//! The per-pattern result struct types (`RetryLoop`, `FailureRun`, …) live
//! in `findings.rs` per AgentWorkforce/burn#268's deferred-types decision,
//! so this module re-exports them rather than redefining the same shapes.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::reader::{
    CompactionEvent, ContentKind, ContentRecord, ContentToolResult, ContentToolUse, ToolCall,
    ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord, UserTurnRecord,
};
use serde_json::Value;

use crate::analyze::cost::sum_turn_costs;
use crate::analyze::findings::{
    CancellationRun, CompactionLoss, EditHeavySession, EditPreview, EditRevertCycle, FailureRun,
    PatternEventSource, PatternsResult, RetryLoop, SessionPatternSummary, SkillPruningProtection,
    SkillRecallDup, SystemPromptTax,
};
use crate::analyze::pricing::PricingTable;
use crate::analyze::util::{group_turns_by_session, stringify_tool_result, truncate_chars};

mod shell;

mod compaction;
mod edits;
mod skills;
mod streaks;

use compaction::detect_compaction_losses;
use edits::{detect_edit_heavy_for_session, detect_edit_reverts_for_session};
use skills::{
    detect_skill_pruning_protection_for_session, detect_skill_recall_dups_for_session,
    detect_system_prompt_tax_for_session,
};
use streaks::{
    detect_failure_runs_for_session, detect_graph_status_patterns_for_session,
    detect_retry_loops_for_session,
};

// ---------------------------------------------------------------------------
// Hardcoded thresholds. Each constant cites the TS source line so future
// updates stay aligned with `packages/analyze/src/patterns.ts`.
// ---------------------------------------------------------------------------

/// Minimum length of an errored same-(tool, args) streak that becomes a
/// retry loop. TS: `MIN_RETRY_LEN = 3` (patterns.ts:247).
const MIN_RETRY_LEN: usize = 3;

/// Minimum length of an errored streak (any tool/args mix) that becomes a
/// failure run. TS: `MIN_FAILURE_RUN_LEN = 3` (patterns.ts:248).
const MIN_FAILURE_RUN_LEN: usize = 3;

/// Truncation limit for `EditRevertCycle.samplePreview` old/new strings
/// per field. TS: `SAMPLE_PREVIEW_MAX_CHARS = 200` (patterns.ts:254).
const SAMPLE_PREVIEW_MAX_CHARS: usize = 200;

/// Edit:read ratio above which a session is flagged edit-heavy. TS:
/// `EDIT_HEAVY_RATIO = 4` (patterns.ts:260).
const EDIT_HEAVY_RATIO: f64 = 4.0;

/// Floor on edit count for the edit-heavy detector — even at infinite
/// ratio, we want at least this many edits to flag. TS:
/// `EDIT_HEAVY_MIN_EDITS = 5` (patterns.ts:261).
const EDIT_HEAVY_MIN_EDITS: u64 = 5;

/// Maximum chars of a leading error line we surface in a retry-loop /
/// failure-run signature. TS: `ERROR_SIGNATURE_MAX_CHARS = 240`
/// (patterns.ts:592).
const ERROR_SIGNATURE_MAX_CHARS: usize = 240;

// Tool-name sets used by edit-heavy / compaction-window detection.
// Kept in sync with `READ_TOOLS` / `EDIT_TOOLS` in patterns.ts:268-269.
const READ_TOOL_NAMES: &[&str] = &["Read", "NotebookRead"];
const EDIT_TOOL_NAMES: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

fn is_read_tool(name: &str) -> bool {
    READ_TOOL_NAMES.contains(&name)
}

fn is_edit_tool(name: &str) -> bool {
    EDIT_TOOL_NAMES.contains(&name)
}

// Codex shell-name recognition (patterns.ts:270): `CODEX_SHELL_NAMES`. The
// companion `CODEX_SHELL_READ_COMMANDS` check lives in the `shell` submodule.
fn is_codex_shell_name(name: &str) -> bool {
    name == "exec_command" || name == "shell"
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Mirrors the TS `DetectPatternsOptions` shape (patterns.ts:229-245). The
/// optional sources of supplemental data are carried by reference so callers
/// can reuse caches without forcing a clone. `pricing` is required and has
/// no sensible default, so callers always supply one.
#[derive(Debug, Clone)]
pub struct DetectPatternsOptions<'a> {
    pub pricing: &'a PricingTable,
    pub compactions: Option<&'a [CompactionEvent]>,
    /// `sessionId -> UserTurnRecord[]` in source order. Drives the
    /// system-prompt-tax first-user-message size estimate.
    pub user_turns_by_session: Option<&'a HashMap<String, Vec<UserTurnRecord>>>,
    /// `sessionId -> ContentRecord[]` in source order. Enriches retry loops,
    /// failure runs, edit-revert cycles, and compaction losses with content-
    /// sidecar fields. Detectors run identically without it.
    pub content_by_session: Option<&'a HashMap<String, Vec<ContentRecord>>>,
    /// Tool-result / subagent / queue / progress chronology. When supplied
    /// for a session, retry/failure/cancellation detectors use it instead of
    /// the legacy `TurnRecord.tool_calls[].is_error` reconstruction.
    pub tool_result_events: Option<&'a [ToolResultEventRecord]>,
}

impl<'a> DetectPatternsOptions<'a> {
    /// Convenience constructor used by tests and embedders that only need
    /// to supply pricing.
    pub fn with_pricing(pricing: &'a PricingTable) -> Self {
        Self {
            pricing,
            compactions: None,
            user_turns_by_session: None,
            content_by_session: None,
            tool_result_events: None,
        }
    }
}

/// Run every detector across the supplied turn stream. Mirrors the TS
/// `detectPatterns` orchestrator (patterns.ts:273-345).
pub fn detect_patterns(turns: &[TurnRecord], opts: &DetectPatternsOptions<'_>) -> PatternsResult {
    let by_session = group_turns_by_session(turns);
    let events_by_session = group_tool_result_events_by_session(opts.tool_result_events);

    let mut retry_loops: Vec<RetryLoop> = Vec::new();
    let mut failure_runs: Vec<FailureRun> = Vec::new();
    let mut cancelled_runs: Vec<CancellationRun> = Vec::new();
    let mut edit_reverts: Vec<EditRevertCycle> = Vec::new();
    let mut skill_recall_dups: Vec<SkillRecallDup> = Vec::new();
    let mut skill_pruning_protection: Vec<SkillPruningProtection> = Vec::new();
    let mut system_prompt_taxes: Vec<SystemPromptTax> = Vec::new();
    let mut edit_heavy_sessions: Vec<EditHeavySession> = Vec::new();

    // Iterate sessions in insertion (= first-seen) order so output ordering
    // matches the TS `Map` iteration contract.
    for (session_id, mut session_turns) in by_session {
        // TS sorts each per-session bucket by turn_index in place. Mirror that.
        session_turns.sort_by_key(|t| t.turn_index);

        let content_index = build_content_index(
            opts.content_by_session
                .and_then(|m| m.get(&session_id))
                .map(|v| v.as_slice()),
        );

        let session_events = events_by_session
            .get(&session_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        if !session_events.is_empty() {
            let graph = detect_graph_status_patterns_for_session(
                &session_id,
                &session_turns,
                session_events,
                opts.pricing,
                content_index.as_ref(),
            );
            retry_loops.extend(graph.retry_loops);
            failure_runs.extend(graph.failure_runs);
            cancelled_runs.extend(graph.cancelled_runs);
        } else {
            retry_loops.extend(detect_retry_loops_for_session(
                &session_id,
                &session_turns,
                opts.pricing,
                content_index.as_ref(),
            ));
            failure_runs.extend(detect_failure_runs_for_session(
                &session_id,
                &session_turns,
                opts.pricing,
                content_index.as_ref(),
            ));
        }

        edit_reverts.extend(detect_edit_reverts_for_session(
            &session_id,
            &session_turns,
            opts.pricing,
            content_index.as_ref(),
        ));
        skill_recall_dups.extend(detect_skill_recall_dups_for_session(
            &session_id,
            &session_turns,
            opts.pricing,
        ));
        skill_pruning_protection.extend(detect_skill_pruning_protection_for_session(
            &session_id,
            &session_turns,
            opts.pricing,
        ));

        let user_turns = opts
            .user_turns_by_session
            .and_then(|m| m.get(&session_id))
            .map(|v| v.as_slice());
        system_prompt_taxes.extend(detect_system_prompt_tax_for_session(
            &session_id,
            &session_turns,
            opts.pricing,
            user_turns,
        ));
        edit_heavy_sessions.extend(detect_edit_heavy_for_session(
            &session_id,
            &session_turns,
            opts.pricing,
        ));
    }

    let compactions = match opts.compactions {
        Some(events) => {
            detect_compaction_losses(events, turns, opts.pricing, opts.content_by_session)
        }
        None => Vec::new(),
    };

    let session_summaries = build_summaries(
        &retry_loops,
        &failure_runs,
        &cancelled_runs,
        &compactions,
        &edit_reverts,
        &skill_recall_dups,
        &skill_pruning_protection,
        &system_prompt_taxes,
        &edit_heavy_sessions,
    );

    PatternsResult {
        retry_loops,
        failure_runs,
        cancelled_runs,
        compactions,
        edit_reverts,
        skill_recall_dups,
        skill_pruning_protection,
        system_prompt_taxes,
        edit_heavy_sessions,
        session_summaries,
    }
}

// ---------------------------------------------------------------------------
// Grouping helpers
// ---------------------------------------------------------------------------

fn group_tool_result_events_by_session<'a>(
    events: Option<&'a [ToolResultEventRecord]>,
) -> HashMap<String, Vec<&'a ToolResultEventRecord>> {
    let mut by: HashMap<String, Vec<&'a ToolResultEventRecord>> = HashMap::new();
    if let Some(events) = events {
        for e in events {
            by.entry(e.session_id.clone()).or_default().push(e);
        }
    }
    by
}

// ---------------------------------------------------------------------------
// Iterators / refs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ToolCallRef<'a> {
    turn: &'a TurnRecord,
    call: &'a ToolCall,
}

fn flatten_tool_calls<'a>(turns: &'a [&'a TurnRecord]) -> Vec<ToolCallRef<'a>> {
    let mut out: Vec<ToolCallRef<'a>> = Vec::new();
    for turn in turns {
        for call in &turn.tool_calls {
            out.push(ToolCallRef { turn, call });
        }
    }
    out
}

#[derive(Clone)]
struct ToolResultEventRef<'a> {
    event: &'a ToolResultEventRecord,
    turn: Option<&'a TurnRecord>,
    call: Option<&'a ToolCall>,
    tool: String,
    target: Option<String>,
    args_hash: Option<String>,
    turn_index: u64,
}

fn build_terminal_event_refs<'a>(
    session_id: &str,
    turns: &[&'a TurnRecord],
    events: &[&'a ToolResultEventRecord],
) -> Vec<ToolResultEventRef<'a>> {
    let mut by_tool_use_id: HashMap<&str, ToolCallRef<'a>> = HashMap::new();
    let mut turn_by_message_id: HashMap<&str, &'a TurnRecord> = HashMap::new();
    for turn in turns {
        turn_by_message_id.insert(turn.message_id.as_str(), turn);
        for call in &turn.tool_calls {
            by_tool_use_id.insert(call.id.as_str(), ToolCallRef { turn, call });
        }
    }

    // Collapse progress/fanout rows to the final observed status for each
    // toolUseId. A trailing `running` row means no terminal status landed
    // yet, so it is excluded below. We must walk in chronological order so
    // a later terminal-status row overwrites an earlier intermediate one —
    // mirrors `[...events].sort(compareToolResultEvents)` in TS.
    let mut sorted = events.to_vec();
    sorted.sort_by(|a, b| compare_tool_result_events(a, b));

    // TS uses an insertion-ordered Map; reproduce that with a (Vec keys,
    // HashMap last-seen) pair.
    let mut order_keys: Vec<&str> = Vec::new();
    let mut last_seen: HashMap<&str, &ToolResultEventRecord> = HashMap::new();
    for event in &sorted {
        if event.session_id != session_id {
            continue;
        }
        if !last_seen.contains_key(event.tool_use_id.as_str()) {
            order_keys.push(event.tool_use_id.as_str());
        }
        last_seen.insert(event.tool_use_id.as_str(), *event);
    }

    let mut collapsed: Vec<&ToolResultEventRecord> = Vec::with_capacity(order_keys.len());
    for k in &order_keys {
        if let Some(e) = last_seen.get(k) {
            collapsed.push(*e);
        }
    }
    // TS then re-sorts the collapsed values chronologically before output:
    //   [...terminalByToolUseId.values()].sort(compareToolResultEvents)
    collapsed.sort_by(|a, b| compare_tool_result_events(a, b));

    let mut out: Vec<ToolResultEventRef<'a>> = Vec::with_capacity(collapsed.len());
    for event in collapsed {
        if matches!(event.status, ToolResultStatus::Running) {
            continue;
        }
        let call_ref = by_tool_use_id.get(event.tool_use_id.as_str()).copied();
        let turn = call_ref
            .map(|c| c.turn)
            .or_else(|| find_turn_for_event(event, turns, &turn_by_message_id));
        out.push(ToolResultEventRef {
            event,
            turn,
            call: call_ref.map(|c| c.call),
            tool: display_tool_name(event, call_ref.map(|c| c.call)),
            target: call_ref.and_then(|c| c.call.target.clone()),
            args_hash: call_ref.map(|c| c.call.args_hash.clone()),
            turn_index: turn.map(|t| t.turn_index).unwrap_or(event.event_index),
        });
    }
    out
}

fn compare_tool_result_events(
    a: &ToolResultEventRecord,
    b: &ToolResultEventRecord,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a.event_index != b.event_index {
        return a.event_index.cmp(&b.event_index);
    }
    let a_ts = a.ts.as_deref().unwrap_or("");
    let b_ts = b.ts.as_deref().unwrap_or("");
    let by_ts = a_ts.cmp(b_ts);
    if by_ts != Ordering::Equal {
        return by_ts;
    }
    a.tool_use_id.cmp(&b.tool_use_id)
}

fn find_turn_for_event<'a>(
    event: &ToolResultEventRecord,
    turns: &[&'a TurnRecord],
    turn_by_message_id: &HashMap<&str, &'a TurnRecord>,
) -> Option<&'a TurnRecord> {
    if let Some(message_id) = event.message_id.as_deref() {
        if let Some(t) = turn_by_message_id.get(message_id) {
            return Some(*t);
        }
    }
    let event_ts = event.ts.as_deref()?;
    let mut best: Option<&'a TurnRecord> = None;
    for turn in turns {
        if turn.ts.as_str() <= event_ts {
            best = Some(*turn);
            continue;
        }
        if best.is_none() {
            return Some(*turn);
        }
        break;
    }
    best
}

fn display_tool_name(event: &ToolResultEventRecord, call: Option<&ToolCall>) -> String {
    if let Some(c) = call {
        return c.name.clone();
    }
    match event.event_source {
        ToolResultEventSource::SubagentNotification => "Subagent".to_string(),
        ToolResultEventSource::FunctionCallOutput => "FunctionCall".to_string(),
        ToolResultEventSource::QueueEvent => "QueueEvent".to_string(),
        ToolResultEventSource::ProgressEvent => "ProgressEvent".to_string(),
        ToolResultEventSource::ToolResult => "Tool".to_string(),
    }
}

fn coalesce_event_source(refs: &[ToolResultEventRef<'_>]) -> PatternEventSource {
    let mut sources: HashSet<ToolResultEventSource> = HashSet::new();
    for r in refs {
        sources.insert(r.event.event_source);
    }
    if sources.len() == 1 {
        PatternEventSource::from(refs[0].event.event_source)
    } else {
        PatternEventSource::Mixed
    }
}

fn dedup_defined_turns<'a>(refs: &[ToolResultEventRef<'a>]) -> Vec<&'a TurnRecord> {
    let mut turns: Vec<&'a TurnRecord> = Vec::new();
    for r in refs {
        if let Some(t) = r.turn {
            turns.push(t);
        }
    }
    dedup_turns(turns)
}

fn event_refs_to_tool_call_refs<'a>(refs: &[ToolResultEventRef<'a>]) -> Vec<ToolCallRef<'a>> {
    let mut out: Vec<ToolCallRef<'a>> = Vec::new();
    for r in refs {
        if let (Some(turn), Some(call)) = (r.turn, r.call) {
            out.push(ToolCallRef { turn, call });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Content sidecar index
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct ContentIndex {
    tool_results: HashMap<String, ContentToolResult>,
    tool_uses: HashMap<String, ContentToolUse>,
}

fn build_content_index(records: Option<&[ContentRecord]>) -> Option<ContentIndex> {
    let records = records?;
    if records.is_empty() {
        return None;
    }
    let mut idx = ContentIndex::default();
    for r in records {
        match r.kind {
            ContentKind::ToolResult => {
                if let Some(tr) = &r.tool_result {
                    // Keep first observation per tool_use_id — TS comment in
                    // patterns.ts:551-553.
                    idx.tool_results
                        .entry(tr.tool_use_id.clone())
                        .or_insert_with(|| tr.clone());
                }
            }
            ContentKind::ToolUse => {
                if let Some(tu) = &r.tool_use {
                    idx.tool_uses
                        .entry(tu.id.clone())
                        .or_insert_with(|| tu.clone());
                }
            }
            _ => {}
        }
    }
    Some(idx)
}

fn extract_error_signature(tool_result: Option<&ContentToolResult>) -> Option<String> {
    let tool_result = tool_result?;
    let text = stringify_tool_result(&tool_result.content);
    if text.is_empty() {
        return None;
    }
    for raw_line in text.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // TS uses `line.length` which counts UTF-16 code units. For ASCII
        // fixtures this is identical to the byte-/char-count. We use chars()
        // to avoid splitting multi-byte sequences mid-codepoint while keeping
        // the same threshold semantics for ASCII inputs.
        return Some(truncate_chars(line, ERROR_SIGNATURE_MAX_CHARS));
    }
    None
}

fn truncate_for_preview(s: &str) -> String {
    truncate_chars(s, SAMPLE_PREVIEW_MAX_CHARS)
}

fn extract_edit_preview(input: Option<&BTreeMap<String, Value>>) -> Option<EditPreview> {
    let input = input?;
    let old_raw = input.get("old_string");
    let new_raw = input.get("new_string");
    let content_raw = input.get("content");
    let old_str = match old_raw {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut new_str = match new_raw {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    if new_str.is_empty() {
        if let Some(Value::String(c)) = content_raw {
            new_str = c.clone();
        }
    }
    if old_str.is_empty() && new_str.is_empty() {
        return None;
    }
    Some(EditPreview {
        old: truncate_for_preview(&old_str),
        new: truncate_for_preview(&new_str),
    })
}

fn sum_cost_for_turns(turns: &[&TurnRecord], pricing: &PricingTable) -> f64 {
    sum_turn_costs(turns.iter().copied(), pricing)
}

// ---------------------------------------------------------------------------
// Shared streak-accumulation skeleton
// ---------------------------------------------------------------------------

/// How the streak runner should treat the current element relative to the
/// streak built so far.
enum StreakOp {
    /// Append this element to the current streak.
    Extend,
    /// Commit the current streak, then start a fresh streak holding this
    /// element (a same-tool retry streak hitting a different tool).
    Rotate,
    /// Commit the current streak and drop this element (a boundary marker).
    Break,
}

/// Walk `elements` in order, asking `classify` how each one relates to the
/// in-progress streak, and run `commit` at every streak boundary (and once at
/// the end). `commit` returns `Some(finding)` for a qualifying streak or `None`
/// to drop it, so the per-detector minimum-length and shape guards live there.
///
/// This centralizes the commit-on-boundary control flow that the retry,
/// failure, and cancellation detectors all share; each detector supplies its
/// own `classify` (what extends/breaks a streak) and `commit` (how a streak
/// becomes a finding) over its own element type — the graph detectors over
/// `ToolResultEventRef`, the flat ones over `ToolCallRef`.
fn detect_streaks<E, T>(
    elements: impl IntoIterator<Item = E>,
    classify: impl Fn(Option<&E>, &E) -> StreakOp,
    mut commit: impl FnMut(&[E]) -> Option<T>,
) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    let mut streak: Vec<E> = Vec::new();
    for elem in elements {
        match classify(streak.first(), &elem) {
            StreakOp::Extend => streak.push(elem),
            StreakOp::Rotate => {
                if let Some(found) = commit(&streak) {
                    out.push(found);
                }
                streak.clear();
                streak.push(elem);
            }
            StreakOp::Break => {
                if let Some(found) = commit(&streak) {
                    out.push(found);
                }
                streak.clear();
            }
        }
    }
    if let Some(found) = commit(&streak) {
        out.push(found);
    }
    out
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn dedup_turns<'a>(turns: Vec<&'a TurnRecord>) -> Vec<&'a TurnRecord> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&'a TurnRecord> = Vec::new();
    for t in turns {
        let key = format!("{}|{}", t.session_id, t.message_id);
        if seen.insert(key) {
            out.push(t);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn build_summaries(
    retry_loops: &[RetryLoop],
    failure_runs: &[FailureRun],
    cancelled_runs: &[CancellationRun],
    compactions: &[CompactionLoss],
    edit_reverts: &[EditRevertCycle],
    skill_recall_dups: &[SkillRecallDup],
    skill_pruning_protection: &[SkillPruningProtection],
    system_prompt_taxes: &[SystemPromptTax],
    edit_heavy_sessions: &[EditHeavySession],
) -> Vec<SessionPatternSummary> {
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, SessionPatternSummary> = HashMap::new();

    fn ensure<'b>(
        sid: &str,
        order: &mut Vec<String>,
        by: &'b mut HashMap<String, SessionPatternSummary>,
    ) -> &'b mut SessionPatternSummary {
        if !by.contains_key(sid) {
            order.push(sid.to_string());
            by.insert(
                sid.to_string(),
                SessionPatternSummary {
                    session_id: sid.to_string(),
                    retry_loop_count: 0,
                    failure_run_count: 0,
                    cancellation_run_count: 0,
                    consecutive_failure_max: 0,
                    compaction_count: 0,
                    edit_revert_count: 0,
                    skill_recall_dup_count: 0,
                    skill_pruning_protection_count: 0,
                    system_prompt_tax_count: 0,
                    edit_heavy_count: 0,
                    total_retries: 0,
                    total_pattern_cost: 0.0,
                },
            );
        }
        by.get_mut(sid).unwrap()
    }

    for r in retry_loops {
        let row = ensure(&r.session_id, &mut order, &mut by);
        row.retry_loop_count += 1;
        row.total_retries += r.attempts;
        row.total_pattern_cost += r.cost;
    }
    for f in failure_runs {
        let row = ensure(&f.session_id, &mut order, &mut by);
        row.failure_run_count += 1;
        if f.length > row.consecutive_failure_max {
            row.consecutive_failure_max = f.length;
        }
        row.total_pattern_cost += f.cost;
    }
    for c in cancelled_runs {
        let row = ensure(&c.session_id, &mut order, &mut by);
        row.cancellation_run_count += 1;
        row.total_pattern_cost += c.cost;
    }
    for c in compactions {
        let row = ensure(&c.session_id, &mut order, &mut by);
        row.compaction_count += 1;
        row.total_pattern_cost += c.cache_lost_cost;
    }
    for e in edit_reverts {
        let row = ensure(&e.session_id, &mut order, &mut by);
        row.edit_revert_count += 1;
        row.total_pattern_cost += e.cost;
    }
    for s in skill_recall_dups {
        let row = ensure(&s.session_id, &mut order, &mut by);
        row.skill_recall_dup_count += 1;
        row.total_pattern_cost += s.cost;
    }
    for s in skill_pruning_protection {
        let row = ensure(&s.session_id, &mut order, &mut by);
        row.skill_pruning_protection_count += 1;
        row.total_pattern_cost += s.cost;
    }
    for s in system_prompt_taxes {
        let row = ensure(&s.session_id, &mut order, &mut by);
        row.system_prompt_tax_count += 1;
        row.total_pattern_cost += s.total_cost;
    }
    for e in edit_heavy_sessions {
        let row = ensure(&e.session_id, &mut order, &mut by);
        // Cost intentionally not added to total_pattern_cost — TS comment in
        // patterns.ts:1477-1479: edit-bearing turn costs already feed into
        // edit-revert and retry-loop costs; double-counting is an error.
        row.edit_heavy_count += 1;
    }

    let mut out: Vec<SessionPatternSummary> = order
        .into_iter()
        .map(|sid| by.remove(&sid).unwrap())
        .collect();
    out.sort_by(|a, b| {
        b.total_pattern_cost
            .partial_cmp(&a.total_pattern_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
#[path = "patterns_tests.rs"]
mod patterns_tests;
