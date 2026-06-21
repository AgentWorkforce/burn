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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::reader::{
    count_retries, normalize_tool_name, CompactionEvent, ContentKind, ContentRecord,
    ContentToolResult, ContentToolUse, SourceKind, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, UserTurnRecord,
};
use serde_json::Value;

use crate::analyze::cost::{cost_for_turn, cost_for_usage, CostForUsageOptions};
use crate::analyze::findings::{
    CancellationRun, CompactionLoss, CompactionLostWork, EditHeavySession, EditPreview,
    EditRevertCycle, EditRevertSamplePreview, FailureRun, FailureRunErrorSignature,
    PatternEventSource, PatternsResult, RetryLoop, SessionPatternSummary, SkillPruningProtection,
    SkillRecallDup, SystemPromptTax,
};
use crate::analyze::pricing::PricingTable;
use crate::analyze::util::group_turns_by_session;

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

// Codex shell-read recognition (patterns.ts:270-271). Mirrors:
//   CODEX_SHELL_NAMES = {'exec_command', 'shell'}
//   CODEX_SHELL_READ_COMMANDS = {'cat', 'head', 'tail'}
fn is_codex_shell_name(name: &str) -> bool {
    name == "exec_command" || name == "shell"
}

fn is_codex_shell_read_command(name: &str) -> bool {
    matches!(name, "cat" | "head" | "tail")
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

struct GraphStatusPatterns {
    retry_loops: Vec<RetryLoop>,
    failure_runs: Vec<FailureRun>,
    cancelled_runs: Vec<CancellationRun>,
}

fn detect_graph_status_patterns_for_session<'a>(
    session_id: &str,
    turns: &[&'a TurnRecord],
    events: &[&'a ToolResultEventRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> GraphStatusPatterns {
    let terminal_refs = build_terminal_event_refs(session_id, turns, events);
    GraphStatusPatterns {
        retry_loops: detect_graph_retry_loops_for_session(
            session_id,
            &terminal_refs,
            pricing,
            content_index,
        ),
        failure_runs: detect_graph_failure_runs_for_session(
            session_id,
            &terminal_refs,
            pricing,
            content_index,
        ),
        cancelled_runs: detect_graph_cancellation_runs_for_session(
            session_id,
            &terminal_refs,
            pricing,
        ),
    }
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

// Stringify a tool_result content block to plain text for signature
// extraction. Mirrors `stringifyToolResult` in patterns.ts:567-587.
fn stringify_tool_result_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            for block in arr {
                match block {
                    Value::Object(map) => {
                        let kind = map.get("type").and_then(|v| v.as_str());
                        let text = map.get("text").and_then(|v| v.as_str());
                        match (kind, text) {
                            (Some("text"), Some(t)) => parts.push(t.to_string()),
                            _ => parts.push(serde_json::to_string(block).unwrap_or_default()),
                        }
                    }
                    Value::String(s) => parts.push(s.clone()),
                    _ => parts.push(serde_json::to_string(block).unwrap_or_default()),
                }
            }
            parts.join("\n")
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn extract_error_signature(tool_result: Option<&ContentToolResult>) -> Option<String> {
    let tool_result = tool_result?;
    let text = stringify_tool_result_content(&tool_result.content);
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
        let char_count = line.chars().count();
        if char_count <= ERROR_SIGNATURE_MAX_CHARS {
            return Some(line.to_string());
        }
        let truncated: String = line.chars().take(ERROR_SIGNATURE_MAX_CHARS - 1).collect();
        return Some(format!("{truncated}…"));
    }
    None
}

fn truncate_for_preview(s: &str) -> String {
    let char_count = s.chars().count();
    if char_count <= SAMPLE_PREVIEW_MAX_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(SAMPLE_PREVIEW_MAX_CHARS - 1).collect();
    format!("{truncated}…")
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
    let mut sum = 0.0;
    for t in turns {
        if let Some(c) = cost_for_turn(t, pricing) {
            sum += c.total;
        }
    }
    sum
}

// ---------------------------------------------------------------------------
// Graph-backed detectors
// ---------------------------------------------------------------------------

fn detect_graph_retry_loops_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<RetryLoop> {
    let mut loops: Vec<RetryLoop> = Vec::new();
    let mut streak: Vec<ToolResultEventRef<'a>> = Vec::new();

    let commit = |streak: &mut Vec<ToolResultEventRef<'a>>, out: &mut Vec<RetryLoop>| {
        if streak.len() < MIN_RETRY_LEN {
            return;
        }
        let first = streak.first().unwrap();
        let last = streak.last().unwrap();
        let contributing = dedup_defined_turns(streak);
        let mut loop_ = RetryLoop {
            session_id: session_id.to_string(),
            tool: first.tool.clone(),
            target: first.target.clone(),
            args_hash: first.args_hash.clone().unwrap_or_default(),
            attempts: streak.len() as u64,
            start_turn_index: first.turn_index,
            end_turn_index: last.turn_index,
            cost: sum_cost_for_turns(&contributing, pricing),
            error_signature: None,
            event_source: Some(coalesce_event_source(streak)),
        };
        let call_refs = event_refs_to_tool_call_refs(streak);
        if let Some(sig) = retry_loop_signature(&call_refs, content_index) {
            loop_.error_signature = Some(sig);
        }
        out.push(loop_);
    };

    for r in refs {
        let is_errored = matches!(r.event.status, ToolResultStatus::Errored);
        if !is_errored || r.call.is_none() || r.args_hash.is_none() {
            commit(&mut streak, &mut loops);
            streak.clear();
            continue;
        }
        if streak.is_empty() {
            streak.push(r.clone());
            continue;
        }
        let head = streak.first().unwrap();
        if head.tool == r.tool && head.args_hash == r.args_hash {
            streak.push(r.clone());
        } else {
            commit(&mut streak, &mut loops);
            streak.clear();
            streak.push(r.clone());
        }
    }
    commit(&mut streak, &mut loops);
    loops
}

fn detect_graph_failure_runs_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRun> {
    let mut runs: Vec<FailureRun> = Vec::new();
    let mut streak: Vec<ToolResultEventRef<'a>> = Vec::new();

    let commit = |streak: &mut Vec<ToolResultEventRef<'a>>, out: &mut Vec<FailureRun>| {
        if streak.len() < MIN_FAILURE_RUN_LEN {
            return;
        }
        let mut keys: HashSet<String> = HashSet::new();
        for r in streak.iter() {
            keys.insert(status_pattern_key(r));
        }
        let has_non_tool_result = streak
            .iter()
            .any(|r| !matches!(r.event.event_source, ToolResultEventSource::ToolResult));
        // A same-(tool,args) tool_result run is a retry loop. Non-tool_result
        // terminal events (notably subagent notifications) remain failure
        // runs — they represent child invocations ending badly, not a parent
        // retry loop. Mirrors patterns.ts:706-710.
        if keys.len() < 2 && !has_non_tool_result {
            return;
        }
        let first = streak.first().unwrap();
        let last = streak.last().unwrap();
        // First-seen unique tool order.
        let mut tools: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in streak.iter() {
            if seen.insert(r.tool.clone()) {
                tools.push(r.tool.clone());
            }
        }
        let contributing = dedup_defined_turns(streak);
        let mut run = FailureRun {
            session_id: session_id.to_string(),
            length: streak.len() as u64,
            start_turn_index: first.turn_index,
            end_turn_index: last.turn_index,
            tools_involved: tools,
            cost: sum_cost_for_turns(&contributing, pricing),
            error_signatures: None,
            event_source: Some(coalesce_event_source(streak)),
        };
        let call_refs = event_refs_to_tool_call_refs(streak);
        let sigs = failure_run_signatures(&call_refs, content_index);
        if !sigs.is_empty() {
            run.error_signatures = Some(sigs);
        }
        out.push(run);
    };

    for r in refs {
        if matches!(r.event.status, ToolResultStatus::Errored) {
            streak.push(r.clone());
        } else {
            commit(&mut streak, &mut runs);
            streak.clear();
        }
    }
    commit(&mut streak, &mut runs);
    runs
}

fn detect_graph_cancellation_runs_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
) -> Vec<CancellationRun> {
    let mut runs: Vec<CancellationRun> = Vec::new();
    let mut streak: Vec<ToolResultEventRef<'a>> = Vec::new();

    let commit = |streak: &mut Vec<ToolResultEventRef<'a>>, out: &mut Vec<CancellationRun>| {
        if streak.is_empty() {
            return;
        }
        let first = streak.first().unwrap();
        let last = streak.last().unwrap();
        let mut tools: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in streak.iter() {
            if seen.insert(r.tool.clone()) {
                tools.push(r.tool.clone());
            }
        }
        let contributing = dedup_defined_turns(streak);
        out.push(CancellationRun {
            session_id: session_id.to_string(),
            length: streak.len() as u64,
            start_turn_index: first.turn_index,
            end_turn_index: last.turn_index,
            tools_involved: tools,
            cost: sum_cost_for_turns(&contributing, pricing),
            event_source: coalesce_event_source(streak),
        });
    };

    for r in refs {
        if matches!(r.event.status, ToolResultStatus::Cancelled) {
            streak.push(r.clone());
        } else {
            commit(&mut streak, &mut runs);
            streak.clear();
        }
    }
    commit(&mut streak, &mut runs);
    runs
}

fn status_pattern_key(r: &ToolResultEventRef<'_>) -> String {
    let args = r
        .args_hash
        .clone()
        .unwrap_or_else(|| r.event.tool_use_id.clone());
    format!("{}|{}", r.tool, args)
}

// ---------------------------------------------------------------------------
// Legacy fallback detectors (no event chronology)
// ---------------------------------------------------------------------------

pub(crate) fn detect_retry_loops_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<RetryLoop> {
    let flat = flatten_tool_calls(turns);
    let mut loops: Vec<RetryLoop> = Vec::new();
    let mut streak: Vec<ToolCallRef<'a>> = Vec::new();

    let commit = |streak: &mut Vec<ToolCallRef<'a>>, out: &mut Vec<RetryLoop>| {
        if streak.len() < MIN_RETRY_LEN {
            return;
        }
        let first = streak.first().unwrap();
        let last = streak.last().unwrap();
        let turns_in_streak: Vec<&TurnRecord> = streak.iter().map(|r| r.turn).collect();
        let contributing = dedup_turns(turns_in_streak);
        let mut loop_ = RetryLoop {
            session_id: session_id.to_string(),
            tool: first.call.name.clone(),
            target: first.call.target.clone(),
            args_hash: first.call.args_hash.clone(),
            attempts: streak.len() as u64,
            start_turn_index: first.turn.turn_index,
            end_turn_index: last.turn.turn_index,
            cost: sum_cost_for_turns(&contributing, pricing),
            error_signature: None,
            event_source: None,
        };
        if let Some(sig) = retry_loop_signature(streak, content_index) {
            loop_.error_signature = Some(sig);
        }
        out.push(loop_);
    };

    for r in &flat {
        let is_errored = r.call.is_error == Some(true);
        if !is_errored {
            commit(&mut streak, &mut loops);
            streak.clear();
            continue;
        }
        if streak.is_empty() {
            streak.push(*r);
            continue;
        }
        let head = streak.first().unwrap().call;
        if head.name == r.call.name && head.args_hash == r.call.args_hash {
            streak.push(*r);
        } else {
            commit(&mut streak, &mut loops);
            streak.clear();
            streak.push(*r);
        }
    }
    commit(&mut streak, &mut loops);
    loops
}

fn retry_loop_signature(
    streak: &[ToolCallRef<'_>],
    content_index: Option<&ContentIndex>,
) -> Option<String> {
    let idx = content_index?;
    let mut first_sig: Option<String> = None;
    let mut diverged = false;
    for r in streak {
        let result = idx.tool_results.get(&r.call.id);
        let sig = extract_error_signature(result);
        let Some(sig) = sig else { continue };
        match &first_sig {
            None => first_sig = Some(sig),
            Some(existing) => {
                if existing != &sig {
                    diverged = true;
                    break;
                }
            }
        }
    }
    let first = first_sig?;
    if diverged {
        Some(format!("{first} (signatures diverged)"))
    } else {
        Some(first)
    }
}

pub(crate) fn detect_failure_runs_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRun> {
    let flat = flatten_tool_calls(turns);
    let mut runs: Vec<FailureRun> = Vec::new();
    let mut streak: Vec<ToolCallRef<'a>> = Vec::new();

    let commit = |streak: &mut Vec<ToolCallRef<'a>>, out: &mut Vec<FailureRun>| {
        if streak.len() < MIN_FAILURE_RUN_LEN {
            return;
        }
        let mut keys: HashSet<String> = HashSet::new();
        for r in streak.iter() {
            keys.insert(format!("{}|{}", r.call.name, r.call.args_hash));
        }
        // Same-(tool,args) run is a retry loop, not a failure run. See
        // patterns.ts:868-872.
        if keys.len() < 2 {
            return;
        }
        let first = streak.first().unwrap();
        let last = streak.last().unwrap();
        let mut tools: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in streak.iter() {
            if seen.insert(r.call.name.clone()) {
                tools.push(r.call.name.clone());
            }
        }
        let turns_in_streak: Vec<&TurnRecord> = streak.iter().map(|r| r.turn).collect();
        let contributing = dedup_turns(turns_in_streak);
        let mut run = FailureRun {
            session_id: session_id.to_string(),
            length: streak.len() as u64,
            start_turn_index: first.turn.turn_index,
            end_turn_index: last.turn.turn_index,
            tools_involved: tools,
            cost: sum_cost_for_turns(&contributing, pricing),
            error_signatures: None,
            event_source: None,
        };
        let sigs = failure_run_signatures(streak, content_index);
        if !sigs.is_empty() {
            run.error_signatures = Some(sigs);
        }
        out.push(run);
    };

    for r in &flat {
        if r.call.is_error == Some(true) {
            streak.push(*r);
        } else {
            commit(&mut streak, &mut runs);
            streak.clear();
        }
    }
    commit(&mut streak, &mut runs);
    runs
}

fn failure_run_signatures(
    streak: &[ToolCallRef<'_>],
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRunErrorSignature> {
    let Some(idx) = content_index else {
        return Vec::new();
    };
    let mut out: Vec<FailureRunErrorSignature> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in streak {
        if seen.contains(&r.call.name) {
            continue;
        }
        let result = idx.tool_results.get(&r.call.id);
        let Some(sig) = extract_error_signature(result) else {
            continue;
        };
        out.push(FailureRunErrorSignature {
            tool: r.call.name.clone(),
            first_line: sig,
        });
        seen.insert(r.call.name.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Edit revert detector
// ---------------------------------------------------------------------------

pub(crate) fn detect_edit_reverts_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<EditRevertCycle> {
    struct EditSlot<'a> {
        pre_hash: Option<String>,
        post_hash: Option<String>,
        turn: &'a TurnRecord,
        tool_use_id: String,
    }
    let mut by_file: HashMap<String, Vec<EditSlot<'a>>> = HashMap::new();
    let mut cycles: Vec<EditRevertCycle> = Vec::new();

    let flat = flatten_tool_calls(turns);
    for r in &flat {
        let call = r.call;
        let Some(target) = call.target.as_deref() else {
            continue;
        };
        if call.name != "Edit" && call.name != "Write" && call.name != "NotebookEdit" {
            continue;
        }
        // Failed edits don't actually change file state. Mirrors patterns.ts:951-952.
        if call.is_error == Some(true) {
            continue;
        }
        let slot = EditSlot {
            pre_hash: call.edit_pre_hash.clone(),
            post_hash: call.edit_post_hash.clone(),
            turn: r.turn,
            tool_use_id: call.id.clone(),
        };
        let history = by_file.entry(target.to_string()).or_default();
        if let Some(post_hash) = &slot.post_hash {
            let match_idx = history
                .iter()
                .position(|prior| prior.pre_hash.as_deref() == Some(post_hash.as_str()));
            if let Some(idx) = match_idx {
                let first = &history[idx];
                let mut cycle = EditRevertCycle {
                    session_id: session_id.to_string(),
                    file_path: target.to_string(),
                    first_edit_turn_index: first.turn.turn_index,
                    revert_turn_index: r.turn.turn_index,
                    span_turns: r.turn.turn_index - first.turn.turn_index,
                    cost: sum_cost_for_turns(&dedup_turns(vec![first.turn, r.turn]), pricing),
                    sample_preview: None,
                };
                if let Some(content_idx) = content_index {
                    let first_edit = extract_edit_preview(
                        content_idx
                            .tool_uses
                            .get(&first.tool_use_id)
                            .map(|tu| &tu.input),
                    );
                    let revert = extract_edit_preview(
                        content_idx
                            .tool_uses
                            .get(&slot.tool_use_id)
                            .map(|tu| &tu.input),
                    );
                    if let (Some(first_edit), Some(revert)) = (first_edit, revert) {
                        cycle.sample_preview = Some(EditRevertSamplePreview { first_edit, revert });
                    }
                }
                cycles.push(cycle);
                // Reset the file's history. patterns.ts:982-984.
                by_file.insert(target.to_string(), Vec::new());
                continue;
            }
        }
        history.push(slot);
    }
    cycles
}

// ---------------------------------------------------------------------------
// Compaction loss detector
// ---------------------------------------------------------------------------

fn detect_compaction_losses(
    events: &[CompactionEvent],
    turns: &[TurnRecord],
    pricing: &PricingTable,
    content_by_session: Option<&HashMap<String, Vec<ContentRecord>>>,
) -> Vec<CompactionLoss> {
    // turn_by_message_id over the full input for cache pricing lookup.
    let mut turn_by_message_id: HashMap<&str, &TurnRecord> = HashMap::new();
    for t in turns {
        turn_by_message_id.insert(t.message_id.as_str(), t);
    }

    // Group events by session in arrival order.
    let mut events_order: Vec<String> = Vec::new();
    let mut events_by_session: HashMap<String, Vec<&CompactionEvent>> = HashMap::new();
    for e in events {
        if !events_by_session.contains_key(&e.session_id) {
            events_order.push(e.session_id.clone());
        }
        events_by_session
            .entry(e.session_id.clone())
            .or_default()
            .push(e);
    }
    for list in events_by_session.values_mut() {
        list.sort_by(|a, b| a.ts.cmp(&b.ts));
    }

    // Sort turns by session, then turn_index.
    let mut turns_by_session: HashMap<String, Vec<&TurnRecord>> = HashMap::new();
    for t in turns {
        turns_by_session
            .entry(t.session_id.clone())
            .or_default()
            .push(t);
    }
    for list in turns_by_session.values_mut() {
        list.sort_by_key(|t| t.turn_index);
    }

    let mut prev_boundary_ts: HashMap<String, String> = HashMap::new();
    let mut out: Vec<CompactionLoss> = Vec::new();

    for sid in &events_order {
        let session_events = events_by_session.get(sid).unwrap();
        for e in session_events {
            let tokens = e.tokens_before_compact.unwrap_or(0);
            let mut cache_lost_cost = 0.0_f64;
            if tokens > 0 {
                if let Some(precid) = e.preceding_message_id.as_deref() {
                    if let Some(preceding) = turn_by_message_id.get(precid) {
                        let usage = crate::reader::Usage {
                            input: 0,
                            output: 0,
                            reasoning: 0,
                            cache_read: tokens,
                            cache_create_5m: 0,
                            cache_create_1h: 0,
                        };
                        if let Some(priced) = cost_for_usage(
                            &usage,
                            &preceding.model,
                            pricing,
                            CostForUsageOptions::default(),
                        ) {
                            cache_lost_cost = priced.total;
                        }
                    }
                }
            }
            let mut loss = CompactionLoss {
                session_id: e.session_id.clone(),
                ts: e.ts.clone(),
                preceding_message_id: e.preceding_message_id.clone(),
                tokens_before_compact: tokens,
                cache_lost_cost,
                lost_work: None,
            };
            // Gate on content-sidecar presence — `lost_work` is the "with
            // content" enrichment. Mirrors patterns.ts:1066-1074.
            if let Some(map) = content_by_session {
                if map.contains_key(&e.session_id) {
                    let session_turns = turns_by_session
                        .get(&e.session_id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let window_start = prev_boundary_ts.get(&e.session_id).cloned();
                    loss.lost_work = Some(summarize_compacted_window(
                        session_turns,
                        window_start.as_deref(),
                        &e.ts,
                    ));
                }
            }
            out.push(loss);
            prev_boundary_ts.insert(e.session_id.clone(), e.ts.clone());
        }
    }
    out
}

fn summarize_compacted_window(
    session_turns: &[&TurnRecord],
    window_start: Option<&str>,
    boundary_ts: &str,
) -> CompactionLostWork {
    let mut bash_count: u64 = 0;
    let mut edit_count: u64 = 0;
    let mut read_count: u64 = 0;
    let mut files: BTreeSet<String> = BTreeSet::new();
    for t in session_turns {
        if let Some(ws) = window_start {
            if t.ts.as_str() <= ws {
                continue;
            }
        }
        if t.ts.as_str() > boundary_ts {
            continue;
        }
        for call in &t.tool_calls {
            let name = normalize_tool_name(&call.name);
            if name == "Bash" {
                bash_count += 1;
            } else if is_edit_tool(name) {
                edit_count += 1;
                if let Some(target) = &call.target {
                    files.insert(target.clone());
                }
            } else if is_read_tool(name) {
                read_count += 1;
            }
        }
    }
    CompactionLostWork {
        files: files.into_iter().collect(),
        bash_count,
        edit_count,
        read_count,
    }
}

// ---------------------------------------------------------------------------
// OpenCode skill detectors
// ---------------------------------------------------------------------------

pub(crate) fn detect_skill_recall_dups_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<SkillRecallDup> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<ToolCallRef<'_>>> = HashMap::new();
    let flat = flatten_tool_calls(turns);
    for r in &flat {
        if r.call.name != "skill" {
            continue;
        }
        let Some(skill_name) = r.call.skill_name.as_deref() else {
            continue;
        };
        if !by_name.contains_key(skill_name) {
            order.push(skill_name.to_string());
        }
        by_name.entry(skill_name.to_string()).or_default().push(*r);
    }
    let mut out: Vec<SkillRecallDup> = Vec::new();
    for name in order {
        let refs = by_name.get(&name).unwrap();
        if refs.len() < 2 {
            continue;
        }
        let first = refs.first().unwrap();
        let last = refs.last().unwrap();
        let turns_in_streak: Vec<&TurnRecord> = refs.iter().map(|r| r.turn).collect();
        let contributing = dedup_turns(turns_in_streak);
        out.push(SkillRecallDup {
            session_id: session_id.to_string(),
            skill_name: name,
            call_count: refs.len() as u64,
            first_turn_index: first.turn.turn_index,
            last_turn_index: last.turn.turn_index,
            cost: sum_cost_for_turns(&contributing, pricing),
        });
    }
    out
}

pub(crate) fn detect_skill_pruning_protection_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<SkillPruningProtection> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let mut out: Vec<SkillPruningProtection> = Vec::new();
    let flat = flatten_tool_calls(turns);
    for r in &flat {
        if r.call.name != "skill" {
            continue;
        }
        let Some(skill_name) = r.call.skill_name.clone() else {
            continue;
        };
        let invoke_index = r.turn.turn_index;
        let mut riding_turns = 0_u64;
        let mut last_cached_turn_index = invoke_index;
        let mut riding_cost = 0.0_f64;
        for t in turns {
            if t.turn_index <= invoke_index {
                continue;
            }
            if t.usage.cache_read > 0 {
                riding_turns += 1;
                last_cached_turn_index = t.turn_index;
                if let Some(c) = cost_for_turn(t, pricing) {
                    riding_cost += c.total;
                }
            }
        }
        if riding_turns == 0 {
            continue;
        }
        let invoke_cost = cost_for_turn(r.turn, pricing)
            .map(|c| c.total)
            .unwrap_or(0.0);
        out.push(SkillPruningProtection {
            session_id: session_id.to_string(),
            skill_name,
            invoked_turn_index: invoke_index,
            riding_turns,
            last_cached_turn_index,
            cost: invoke_cost + riding_cost,
        });
    }
    out
}

pub(crate) fn detect_system_prompt_tax_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
    user_turns: Option<&[UserTurnRecord]>,
) -> Vec<SystemPromptTax> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let first_turn = turns[0];
    let first_cache_create = first_turn.usage.cache_create_5m + first_turn.usage.cache_create_1h;
    if first_cache_create == 0 {
        return Vec::new();
    }
    let mut first_user_tokens = 0_u64;
    if let Some(ut) = user_turns {
        if let Some(first_user_turn) = ut.first() {
            for block in &first_user_turn.blocks {
                first_user_tokens += block.approx_tokens;
            }
        }
    }
    if first_user_tokens == 0 {
        return Vec::new();
    }
    let system_prompt_tokens = first_cache_create.saturating_sub(first_user_tokens);
    if system_prompt_tokens == 0 {
        return Vec::new();
    }

    let mut riding_turns = 0_u64;
    let mut total_cost = 0.0_f64;
    for t in turns {
        // Skip the first turn — its cost is the cacheCreate, not the riding
        // tax (patterns.ts:1241-1243).
        if t.message_id == first_turn.message_id && t.turn_index == first_turn.turn_index {
            continue;
        }
        if t.usage.cache_read > 0 {
            riding_turns += 1;
            if let Some(c) = cost_for_turn(t, pricing) {
                total_cost += c.total;
            }
        }
    }
    if riding_turns == 0 {
        return Vec::new();
    }
    vec![SystemPromptTax {
        session_id: session_id.to_string(),
        first_turn_cache_create: first_cache_create,
        first_user_message_tokens: first_user_tokens,
        estimated_system_prompt_tokens: system_prompt_tokens,
        riding_turns,
        total_cost,
    }]
}

// ---------------------------------------------------------------------------
// Edit-heavy detector
// ---------------------------------------------------------------------------

pub(crate) fn detect_edit_heavy_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<EditHeavySession> {
    if turns.is_empty() {
        return Vec::new();
    }
    let mut read_count: u64 = 0;
    let mut edit_count: u64 = 0;
    let mut likely_retries: u64 = 0;
    let mut edit_turns: Vec<&TurnRecord> = Vec::new();

    for t in turns {
        let mut turn_has_edit = false;
        for call in &t.tool_calls {
            let name = normalize_tool_name(&call.name);
            if is_read_for_edit_heavy(call, t.source) {
                read_count += 1;
            } else if is_edit_tool(name) {
                edit_count += 1;
                turn_has_edit = true;
            }
        }
        if turn_has_edit {
            edit_turns.push(*t);
        }
        likely_retries += count_retries(&t.tool_calls);
    }

    if edit_count < EDIT_HEAVY_MIN_EDITS {
        return Vec::new();
    }
    let ratio = if read_count == 0 {
        f64::INFINITY
    } else {
        edit_count as f64 / read_count as f64
    };
    if ratio <= EDIT_HEAVY_RATIO {
        return Vec::new();
    }
    vec![EditHeavySession {
        source: turns[0].source,
        session_id: session_id.to_string(),
        read_count,
        edit_count,
        ratio,
        likely_retries,
        cost: sum_cost_for_turns(&dedup_turns(edit_turns), pricing),
    }]
}

fn is_read_for_edit_heavy(call: &ToolCall, source: SourceKind) -> bool {
    if is_read_tool(normalize_tool_name(&call.name)) {
        return true;
    }
    source == SourceKind::Codex && is_codex_shell_file_read(call)
}

fn is_codex_shell_file_read(call: &ToolCall) -> bool {
    if !is_codex_shell_name(&call.name) {
        return false;
    }
    let Some(target) = call.target.as_deref() else {
        return false;
    };
    shell_command_has_file_read(target)
}

fn shell_command_has_file_read(command: &str) -> bool {
    for segment in split_shell_segments(command) {
        if shell_segment_starts_with_file_read(segment) {
            return true;
        }
    }
    false
}

// Mirrors `command.split(/(?:&&|\|\||;|\n)/)` from patterns.ts:1318.
fn split_shell_segments(command: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0_usize;
    let mut i = 0_usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' || b == b';' {
            out.push(&command[start..i]);
            start = i + 1;
            i += 1;
            continue;
        }
        if i + 1 < bytes.len()
            && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
        {
            out.push(&command[start..i]);
            start = i + 2;
            i += 2;
            continue;
        }
        i += 1;
    }
    out.push(&command[start..]);
    out
}

fn shell_segment_starts_with_file_read(segment: &str) -> bool {
    let tokens = shell_words(segment);
    let mut i = 0_usize;
    while i < tokens.len() && is_shell_env_assignment(&tokens[i]) {
        i += 1;
    }
    if i >= tokens.len() {
        return false;
    }
    let cmd = command_basename(&tokens[i]);
    if !is_codex_shell_read_command(&cmd) {
        return false;
    }
    let rest: Vec<String> = tokens[i + 1..].to_vec();
    has_shell_file_operand(&cmd, &rest)
}

// Mirrors the JS regex `/"[^"]*"|'[^']*'|\S+/g` from patterns.ts:1336.
fn shell_words(segment: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = segment.chars().collect();
    let mut i = 0_usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            // Include closing quote if present, mirroring `"[^"]*"` regex
            // semantics — match consumes the closing quote.
            if i < chars.len() {
                i += 1;
                out.push(chars[start..i].iter().collect());
            } else {
                // Unterminated quote — JS regex would not match. Fall back
                // to a `\S+` style read of the remainder so we don't drop
                // the token entirely.
                let mut j = start;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                out.push(chars[start..j].iter().collect());
                i = j;
            }
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

fn is_shell_env_assignment(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let mut saw_eq = false;
    for c in chars {
        if c == '=' {
            saw_eq = true;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    saw_eq
}

fn command_basename(token: &str) -> String {
    let unquoted = strip_shell_quotes(token);
    match unquoted.rfind('/') {
        Some(i) => unquoted[i + 1..].to_string(),
        None => unquoted,
    }
}

fn strip_shell_quotes(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() >= 2 {
        let first = chars[0];
        let last = chars[chars.len() - 1];
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return chars[1..chars.len() - 1].iter().collect();
        }
    }
    token.to_string()
}

fn has_shell_file_operand(command: &str, tokens: &[String]) -> bool {
    let mut skip_next = false;
    for raw in tokens {
        let token = strip_shell_quotes(raw);
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "|" || token == "&&" || token == "||" || token == ";" {
            break;
        }
        // `/^\d*>/.test(token) || token.startsWith('>')`
        if is_redirect_open(&token) {
            // `/^\d*>+$/.test(token) || /^>+$/.test(token)`
            if is_pure_redirect(&token) {
                skip_next = true;
            }
            continue;
        }
        if token.starts_with('<') {
            continue;
        }
        if token == "-" {
            continue;
        }
        if (command == "head" || command == "tail")
            && (token == "-n" || token == "-c" || token == "--lines" || token == "--bytes")
        {
            skip_next = true;
            continue;
        }
        if (command == "head" || command == "tail") && is_signed_integer(&token) {
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return true;
    }
    false
}

fn is_redirect_open(token: &str) -> bool {
    // matches `^\d*>` (zero or more digits followed by '>')
    let mut chars = token.chars();
    let mut saw_any = false;
    let mut found_gt = false;
    let mut leading_digits = 0_usize;
    for c in chars.by_ref() {
        if c.is_ascii_digit() && !found_gt {
            leading_digits += 1;
            continue;
        }
        if c == '>' {
            found_gt = true;
            saw_any = true;
            break;
        }
        break;
    }
    let _ = leading_digits;
    if found_gt {
        return saw_any;
    }
    token.starts_with('>')
}

fn is_pure_redirect(token: &str) -> bool {
    // matches `/^\d*>+$/` or `/^>+$/`
    let mut i = 0_usize;
    let bytes = token.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == bytes.len() {
        return false;
    }
    let mut saw_gt = false;
    while i < bytes.len() {
        if bytes[i] != b'>' {
            return false;
        }
        saw_gt = true;
        i += 1;
    }
    saw_gt
}

fn is_signed_integer(token: &str) -> bool {
    // matches `/^[+-]?\d+$/`
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = 0_usize;
    if bytes[0] == b'+' || bytes[0] == b'-' {
        i = 1;
    }
    if i == bytes.len() {
        return false;
    }
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            return false;
        }
        i += 1;
    }
    true
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
