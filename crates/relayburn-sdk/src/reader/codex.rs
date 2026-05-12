//! Codex session parser — Rust port of `packages/reader/src/codex.ts`.
//!
//! Mirrors the TS state-machine field-for-field: a single forward pass over
//! line-delimited JSON records, with all per-turn output buffered and only
//! committed at `task_complete` boundaries. Resume state is round-tripped as
//! [`CodexResumeState`] so an incremental ingest can pick up where the last
//! committed turn left off without re-reading prior bytes.
//!
//! Note: the TS parser defaults to a `cl100k` tokenizer for `UserTurnBlock`
//! sizing. The Rust port uses [`HeuristicCounter`] (bytes/4) — see #246 for
//! the cl100k hookup.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::fidelity::classify_fidelity;
use crate::reader::git::ProjectResolver;
use crate::reader::hash::{args_hash, content_hash};
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode, ContentToolResult,
    ContentToolUse, Coverage, Fidelity, RelationshipSourceKind, RelationshipType,
    SessionRelationshipRecord, SourceKind, ToolCall, ToolResultEventRecord, ToolResultEventSource,
    ToolResultStatus, TurnRecord, Usage, UsageGranularity, UserTurnBlock, UserTurnBlockKind,
    UserTurnRecord,
};
use crate::reader::user_turn::{HeuristicCounter, UserTurnTokenizer};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ParseCodexOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexIncrementalOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
    pub start_offset: Option<u64>,
    pub resume: Option<CodexResumeState>,
}

#[derive(Debug, Clone, Default)]
pub struct CumulativeUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub reasoning: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PersistedUserTurnSlot {
    pub blocks: Vec<UserTurnBlock>,
    pub preceding_message_id: Option<String>,
    pub ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLastCompletedTurn {
    pub message_id: String,
    pub cache_read: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CodexTurnContext {
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexResumeState {
    pub cumulative: CumulativeUsage,
    pub session_id: String,
    pub session_cwd: Option<String>,
    pub turn_contexts: HashMap<String, CodexTurnContext>,
    pub user_turn_slot: Option<PersistedUserTurnSlot>,
    pub root_session_emitted: bool,
    pub session_meta_relationship_keys: Vec<String>,
    pub next_event_index: u64,
    pub tool_result_counters: HashMap<String, u64>,
    pub last_completed_turn: Option<CodexLastCompletedTurn>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexIncrementalResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub end_offset: u64,
    pub resume: CodexResumeState,
}

/// Best-effort sniff of the `session_meta.payload.id` from the first JSONL
/// line. Used by ingest to map a renamed-on-disk rollout file back to its
/// canonical session id without parsing the full file.
pub fn read_codex_session_id_hint(file_path: impl AsRef<Path>) -> Option<String> {
    let mut file = File::open(file_path.as_ref()).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let raw = std::str::from_utf8(&buf[..n]).ok()?;
    let first = match raw.find('\n') {
        Some(idx) => &raw[..idx],
        None => raw,
    };
    let first = first.trim_end_matches('\r').trim();
    if first.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(first).ok()?;
    if parsed.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = parsed.get("payload")?;
    session_meta_payload_id(payload)
}

pub fn parse_codex_session(
    file_path: impl AsRef<Path>,
    options: &ParseCodexOptions,
) -> std::io::Result<ParseCodexResult> {
    let inc_opts = ParseCodexIncrementalOptions {
        session_path: options.session_path.clone(),
        content_mode: options.content_mode,
        tokenizer: options.tokenizer,
        start_offset: Some(0),
        resume: None,
    };
    parse_codex_session_incremental(file_path, &inc_opts).map(ParseCodexResult::from)
}

impl From<ParseCodexIncrementalResult> for ParseCodexResult {
    fn from(r: ParseCodexIncrementalResult) -> Self {
        Self {
            turns: r.turns,
            content: r.content,
            events: r.events,
            user_turns: r.user_turns,
            relationships: r.relationships,
            tool_result_events: r.tool_result_events,
        }
    }
}

pub fn parse_codex_session_incremental(
    file_path: impl AsRef<Path>,
    options: &ParseCodexIncrementalOptions,
) -> std::io::Result<ParseCodexIncrementalResult> {
    // The TS parser defaults to cl100k; the Rust port only ships
    // `HeuristicCounter` until a tiktoken-equivalent lands (#246). Honor the
    // option by accepting `None` / `Some(Heuristic)` and rejecting
    // `Some(Cl100k)` with a clear error rather than silently falling back.
    resolve_token_counter(options.tokenizer)?;
    let start_offset = options.start_offset.unwrap_or(0);
    let mut file = File::open(file_path.as_ref())?;
    let size = file.metadata()?.len();
    if start_offset >= size {
        return Ok(ParseCodexIncrementalResult {
            turns: vec![],
            content: vec![],
            events: vec![],
            user_turns: vec![],
            relationships: vec![],
            tool_result_events: vec![],
            end_offset: start_offset,
            resume: clone_resume(options.resume.as_ref()),
        });
    }
    file.seek(SeekFrom::Start(start_offset))?;
    // Stream from `start_offset` line-by-line. The previous implementation
    // pre-allocated `vec![0u8; (size - start_offset) as usize]` and
    // `read_exact` into it — for a multi-GB session that was a multi-GB
    // up-front allocation. With BufReader + `read_until` only the longest
    // single line stays resident.
    let reader = BufReader::new(file);

    let project_resolver = ProjectResolver::new();
    parse_codex_buffer(reader, start_offset, options, &project_resolver)
}

// ---------------------------------------------------------------------------
// Internal parser state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct UserTurnSlot {
    blocks: Vec<UserTurnBlock>,
    preceding_message_id: Option<String>,
    ts: String,
}

impl UserTurnSlot {
    fn from_persisted(p: &PersistedUserTurnSlot) -> Self {
        Self {
            blocks: p.blocks.clone(),
            preceding_message_id: p.preceding_message_id.clone(),
            ts: p.ts.clone(),
        }
    }
    fn to_persisted(&self) -> PersistedUserTurnSlot {
        PersistedUserTurnSlot {
            blocks: self.blocks.clone(),
            preceding_message_id: self.preceding_message_id.clone(),
            ts: self.ts.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct SpawnCallInfo {
    call_id: String,
    ts: String,
    subagent_type: Option<String>,
    description: Option<String>,
    spawned_agent_id: Option<String>,
    emitted: bool,
}

#[derive(Debug, Clone)]
struct OpenTurn {
    turn_id: String,
    ts: String,
    model: String,
    project: Option<String>,
    start_cumulative: CumulativeUsage,
    tool_calls: Vec<ToolCall>,
    seen_call_ids: BTreeSet<String>,
    files_touched: BTreeSet<String>,
    user_text: String,
    assistant_text: String,
    errored_call_ids: BTreeSet<String>,
    content: Vec<ContentRecord>,
    pending_tool_result_events: Vec<ToolResultEventRecord>,
    pending_relationships: Vec<SessionRelationshipRecord>,
    spawn_calls: HashMap<String, SpawnCallInfo>,
    usage_observed: bool,
}

struct FinalizedTurn {
    turn_id: String,
    ts: String,
    model: String,
    project: Option<String>,
    tool_calls: Vec<ToolCall>,
    files_touched: Vec<String>,
    user_text: String,
    assistant_text: String,
    errored_call_ids: BTreeSet<String>,
    content: Vec<ContentRecord>,
    usage: Usage,
    fidelity: Fidelity,
}

fn finalize_turn(open: OpenTurn, cumulative: &CumulativeUsage) -> FinalizedTurn {
    let usage = Usage {
        input: (cumulative.input - open.start_cumulative.input).max(0) as u64,
        output: (cumulative.output - open.start_cumulative.output).max(0) as u64,
        reasoning: (cumulative.reasoning - open.start_cumulative.reasoning).max(0) as u64,
        cache_read: (cumulative.cache_read - open.start_cumulative.cache_read).max(0) as u64,
        cache_create_5m: 0,
        cache_create_1h: 0,
    };
    let mut files: Vec<String> = open.files_touched.into_iter().collect();
    files.sort();
    FinalizedTurn {
        turn_id: open.turn_id,
        ts: open.ts,
        model: open.model,
        project: open.project,
        tool_calls: open.tool_calls,
        files_touched: files,
        user_text: open.user_text,
        assistant_text: open.assistant_text,
        errored_call_ids: open.errored_call_ids,
        content: open.content,
        usage,
        fidelity: build_codex_fidelity(open.usage_observed),
    }
}

fn build_codex_fidelity(usage_observed: bool) -> Fidelity {
    let coverage = Coverage {
        has_input_tokens: usage_observed,
        has_output_tokens: usage_observed,
        has_reasoning_tokens: usage_observed,
        has_cache_read_tokens: usage_observed,
        has_cache_create_tokens: false,
        has_tool_calls: true,
        has_tool_result_events: true,
        has_session_relationships: true,
        has_raw_content: true,
    };
    let class = classify_fidelity(UsageGranularity::PerTurn, &coverage);
    Fidelity {
        granularity: UsageGranularity::PerTurn,
        coverage,
        class,
    }
}

// ---------------------------------------------------------------------------
// Core parser loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pending<T> {
    offset: u64,
    record: T,
}

fn parse_codex_buffer<R: BufRead>(
    mut reader: R,
    start_offset: u64,
    options: &ParseCodexIncrementalOptions,
    project_resolver: &ProjectResolver,
) -> std::io::Result<ParseCodexIncrementalResult> {
    let capture_content = matches!(options.content_mode, Some(ContentStoreMode::Full));
    // Validated by `resolve_token_counter` at the public entry point.
    let counter = HeuristicCounter;

    let resume = options.resume.as_ref();
    let mut session_id = resume.map(|r| r.session_id.clone()).unwrap_or_default();
    let mut session_cwd: Option<String> = resume.and_then(|r| r.session_cwd.clone());
    let mut turn_contexts: HashMap<String, CodexTurnContext> =
        resume.map(|r| r.turn_contexts.clone()).unwrap_or_default();
    let mut cumulative = resume.map(|r| r.cumulative.clone()).unwrap_or_default();

    let mut open_turn: Option<OpenTurn> = None;
    let mut pending_user_text = String::new();
    let mut pending_content: Vec<ContentRecord> = Vec::new();
    let mut finalized: Vec<FinalizedTurn> = Vec::new();

    let mut user_turn_slot: UserTurnSlot = resume
        .and_then(|r| r.user_turn_slot.as_ref())
        .map(UserTurnSlot::from_persisted)
        .unwrap_or_default();
    let mut user_turns: Vec<UserTurnRecord> = Vec::new();

    let mut root_session_emitted = resume.map(|r| r.root_session_emitted).unwrap_or(false);
    let mut seen_session_meta_keys: BTreeSet<String> = resume
        .map(|r| r.session_meta_relationship_keys.iter().cloned().collect())
        .unwrap_or_default();
    let mut next_event_index = resume.map(|r| r.next_event_index).unwrap_or(0);
    let mut tool_result_counters: HashMap<String, u64> = resume
        .map(|r| r.tool_result_counters.clone())
        .unwrap_or_default();
    let mut last_completed_turn: Option<CodexLastCompletedTurn> =
        resume.and_then(|r| r.last_completed_turn.clone());

    let mut pending_tool_result_events: Vec<Pending<ToolResultEventRecord>> = Vec::new();
    let mut pending_relationships: Vec<Pending<SessionRelationshipRecord>> = Vec::new();
    let mut pending_compactions: Vec<Pending<CompactionEvent>> = Vec::new();

    let mut committed_end_offset = start_offset;
    let mut committed_cumulative = cumulative.clone();
    let mut committed_session_id = session_id.clone();
    let mut committed_session_cwd = session_cwd.clone();
    let mut committed_turn_contexts = turn_contexts.clone();
    let mut committed_finalized_count: usize = 0;
    let mut committed_user_turns_count: usize = 0;
    let mut committed_user_turn_slot = user_turn_slot.clone();
    let mut committed_root_session_emitted = root_session_emitted;
    let mut committed_seen_session_meta_keys = seen_session_meta_keys.clone();
    let mut committed_next_event_index = next_event_index;
    let mut committed_tool_result_counters = tool_result_counters.clone();
    let mut committed_last_completed_turn = last_completed_turn.clone();

    let mut line_buf: Vec<u8> = Vec::new();
    let mut current_offset: u64 = start_offset;
    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        // Drop trailing partial lines — the next incremental call resumes
        // from the committed end offset, which only advances past `\n`.
        if line_buf.last() != Some(&b'\n') {
            break;
        }
        let line_end_offset = current_offset + n as u64;
        current_offset = line_end_offset;
        let text = std::str::from_utf8(&line_buf[..n - 1])
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !parsed.is_object() {
            continue;
        }
        let rec_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let rec_timestamp = parsed
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let payload = match parsed.get("payload") {
            Some(p) if p.is_object() => p,
            _ => continue,
        };

        match rec_type {
            "session_meta" => {
                if let Some(id) = session_meta_payload_id(payload) {
                    session_id = id;
                }
                if let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) {
                    session_cwd = Some(cwd.to_string());
                    if let Some(open) = open_turn.as_mut() {
                        if open.project.is_none() {
                            open.project = Some(cwd.to_string());
                        }
                    }
                }
                if !session_id.is_empty() && !root_session_emitted {
                    root_session_emitted = true;
                    let ts = payload
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or(rec_timestamp);
                    pending_relationships.push(Pending {
                        offset: line_end_offset,
                        record: build_root_relationship(&session_id, ts, payload),
                    });
                }
                if !session_id.is_empty() {
                    for row in build_session_meta_relationships(&session_id, payload, rec_timestamp)
                    {
                        let key = codex_relationship_key(&row);
                        if seen_session_meta_keys.contains(&key) {
                            continue;
                        }
                        seen_session_meta_keys.insert(key);
                        pending_relationships.push(Pending {
                            offset: line_end_offset,
                            record: row,
                        });
                    }
                }
                continue;
            }
            "turn_context" => {
                let ctx = CodexTurnContext {
                    turn_id: payload
                        .get("turn_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    cwd: payload
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    model: payload
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                };
                if let Some(tid) = ctx.turn_id.clone() {
                    turn_contexts.insert(tid.clone(), ctx.clone());
                    if let Some(open) = open_turn.as_mut() {
                        if open.turn_id == tid {
                            if open.model.is_empty() {
                                if let Some(m) = ctx.model.as_deref() {
                                    open.model = m.to_string();
                                }
                            }
                            if open.project.is_none() {
                                if let Some(c) = ctx.cwd.as_deref() {
                                    open.project = Some(c.to_string());
                                }
                            }
                        }
                    }
                }
                continue;
            }
            "compacted" => {
                if !session_id.is_empty() {
                    pending_compactions.push(Pending {
                        offset: line_end_offset,
                        record: build_codex_compaction_event(
                            &session_id,
                            rec_timestamp,
                            last_completed_turn.as_ref(),
                        ),
                    });
                }
                continue;
            }
            "event_msg" => {
                let pl_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match pl_type {
                    "token_count" => {
                        if let Some(total) = payload.get("info").and_then(|i| {
                            if i.is_null() {
                                None
                            } else {
                                i.get("total_token_usage")
                            }
                        }) {
                            let input_total = total
                                .get("input_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let cached = total
                                .get("cached_input_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            cumulative.input = input_total - cached;
                            cumulative.cache_read = cached;
                            cumulative.output = total
                                .get("output_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            cumulative.reasoning = total
                                .get("reasoning_output_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            if let Some(open) = open_turn.as_mut() {
                                open.usage_observed = true;
                            }
                        }
                    }
                    "task_started" => {
                        let ts = rec_timestamp;
                        let turn_id = match payload.get("turn_id").and_then(|v| v.as_str()) {
                            Some(t) => t.to_string(),
                            None => continue,
                        };
                        if let Some(open) = open_turn.take() {
                            finalized.push(finalize_turn(open, &cumulative));
                        }
                        if !user_turn_slot.blocks.is_empty() {
                            user_turns.push(build_codex_user_turn_record(
                                &user_turn_slot,
                                &session_id,
                                &turn_id,
                                ts,
                            ));
                        }
                        user_turn_slot = UserTurnSlot::default();
                        let ctx = turn_contexts.get(&turn_id).cloned();
                        let project = ctx
                            .as_ref()
                            .and_then(|c| c.cwd.clone())
                            .or_else(|| session_cwd.clone());
                        let mut open = OpenTurn {
                            turn_id: turn_id.clone(),
                            ts: ts.to_string(),
                            model: ctx
                                .as_ref()
                                .and_then(|c| c.model.clone())
                                .unwrap_or_default(),
                            project,
                            start_cumulative: cumulative.clone(),
                            tool_calls: vec![],
                            seen_call_ids: BTreeSet::new(),
                            files_touched: BTreeSet::new(),
                            user_text: std::mem::take(&mut pending_user_text),
                            assistant_text: String::new(),
                            errored_call_ids: BTreeSet::new(),
                            content: vec![],
                            pending_tool_result_events: vec![],
                            pending_relationships: vec![],
                            spawn_calls: HashMap::new(),
                            usage_observed: false,
                        };
                        if capture_content && !pending_content.is_empty() {
                            for c in pending_content.iter_mut() {
                                c.message_id = turn_id.clone();
                            }
                            open.content.append(&mut pending_content);
                        }
                        open_turn = Some(open);
                    }
                    "task_complete" => {
                        let payload_turn_id = payload
                            .get("turn_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut took: Option<OpenTurn> = None;
                        if let Some(open) = open_turn.as_ref() {
                            if open.turn_id == payload_turn_id {
                                took = open_turn.take();
                            }
                        }
                        if let Some(mut open) = took {
                            // Patch isError on tool-result blocks accumulated this turn
                            for b in user_turn_slot.blocks.iter_mut() {
                                if matches!(b.kind, UserTurnBlockKind::ToolResult) {
                                    if let Some(id) = &b.tool_use_id {
                                        if open.errored_call_ids.contains(id) {
                                            b.is_error = Some(true);
                                        }
                                    }
                                }
                            }
                            // Drain pending tool result events / relationships
                            let mut events = std::mem::take(&mut open.pending_tool_result_events);
                            for ev in events.iter_mut() {
                                if open.errored_call_ids.contains(&ev.tool_use_id) {
                                    ev.status = ToolResultStatus::Errored;
                                    ev.is_error = Some(true);
                                } else if matches!(ev.status, ToolResultStatus::Unknown) {
                                    ev.status = ToolResultStatus::Completed;
                                }
                            }
                            for ev in events {
                                pending_tool_result_events.push(Pending {
                                    offset: line_end_offset,
                                    record: ev,
                                });
                            }
                            let rels = std::mem::take(&mut open.pending_relationships);
                            for r in rels {
                                pending_relationships.push(Pending {
                                    offset: line_end_offset,
                                    record: r,
                                });
                            }
                            user_turn_slot.preceding_message_id = Some(open.turn_id.clone());
                            let closed = finalize_turn(open, &cumulative);
                            last_completed_turn = Some(CodexLastCompletedTurn {
                                message_id: closed.turn_id.clone(),
                                cache_read: closed.usage.cache_read,
                            });
                            finalized.push(closed);
                            // Commit snapshot
                            committed_end_offset = line_end_offset;
                            committed_cumulative = cumulative.clone();
                            committed_session_id = session_id.clone();
                            committed_session_cwd = session_cwd.clone();
                            committed_turn_contexts = turn_contexts.clone();
                            committed_finalized_count = finalized.len();
                            committed_user_turns_count = user_turns.len();
                            committed_user_turn_slot = user_turn_slot.clone();
                            committed_root_session_emitted = root_session_emitted;
                            committed_seen_session_meta_keys = seen_session_meta_keys.clone();
                            committed_next_event_index = next_event_index;
                            committed_tool_result_counters = tool_result_counters.clone();
                            committed_last_completed_turn = last_completed_turn.clone();
                        }
                    }
                    "patch_apply_end" => {
                        if let Some(open) = open_turn.as_mut() {
                            let turn_id = payload.get("turn_id").and_then(|v| v.as_str());
                            if turn_id != Some(open.turn_id.as_str()) {
                                continue;
                            }
                            let success = payload.get("success").and_then(|v| v.as_bool());
                            if success == Some(false) {
                                if let Some(call_id) =
                                    payload.get("call_id").and_then(|v| v.as_str())
                                {
                                    open.errored_call_ids.insert(call_id.to_string());
                                }
                                continue;
                            }
                            if let Some(changes) =
                                payload.get("changes").and_then(|v| v.as_object())
                            {
                                for file in changes.keys() {
                                    open.files_touched.insert(file.clone());
                                }
                            }
                        }
                    }
                    "exec_command_end" => {
                        if let Some(open) = open_turn.as_mut() {
                            let turn_id = payload.get("turn_id").and_then(|v| v.as_str());
                            if turn_id != Some(open.turn_id.as_str()) {
                                continue;
                            }
                            let exit_code = payload.get("exit_code").and_then(|v| v.as_i64());
                            if let (Some(code), Some(call_id)) =
                                (exit_code, payload.get("call_id").and_then(|v| v.as_str()))
                            {
                                if code != 0 {
                                    open.errored_call_ids.insert(call_id.to_string());
                                }
                            }
                        }
                    }
                    other if is_subagent_terminal_notification(other) => {
                        let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                            Some(c) if !c.is_empty() => c.to_string(),
                            _ => continue,
                        };
                        let entry = tool_result_counters.entry(call_id.clone()).or_insert(0);
                        let call_index = *entry;
                        *entry += 1;
                        let status = subagent_notification_status(payload);
                        let mut ev = ToolResultEventRecord {
                            v: 1,
                            source: SourceKind::Codex,
                            session_id: session_id.clone(),
                            message_id: open_turn.as_ref().map(|o| o.turn_id.clone()),
                            tool_use_id: call_id.clone(),
                            call_index: Some(call_index),
                            event_index: next_event_index,
                            ts: if rec_timestamp.is_empty() {
                                None
                            } else {
                                Some(rec_timestamp.to_string())
                            },
                            status,
                            event_source: ToolResultEventSource::SubagentNotification,
                            content_length: None,
                            content_hash: None,
                            is_error: matches!(status, ToolResultStatus::Errored).then_some(true),
                            usage: None,
                            usage_attribution: None,
                            subagent_session_id: None,
                            agent_id: None,
                            replaced_tools: None,
                            collapsed_calls: None,
                        };
                        next_event_index += 1;
                        let spawned_id =
                            pick_string_field(payload, &["agent_id", "subagent_id", "session_id"]);
                        if let Some(sid) = spawned_id.as_ref() {
                            ev.agent_id = Some(sid.clone());
                            ev.subagent_session_id = Some(sid.clone());
                            if let Some(open) = open_turn.as_mut() {
                                if let Some(spawn) = open.spawn_calls.get_mut(&call_id) {
                                    if spawn.spawned_agent_id.is_none() {
                                        spawn.spawned_agent_id = Some(sid.clone());
                                        let info = spawn.clone();
                                        maybe_emit_spawn_relationship(
                                            open,
                                            &session_id,
                                            &info,
                                            rec_timestamp,
                                        );
                                    }
                                }
                            }
                        }
                        if let Some(open) = open_turn.as_mut() {
                            open.pending_tool_result_events.push(ev);
                        } else {
                            pending_tool_result_events.push(Pending {
                                offset: line_end_offset,
                                record: ev,
                            });
                        }
                    }
                    _ => {}
                }
                continue;
            }
            "response_item" => {
                let item_ts = rec_timestamp;
                let pl_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match pl_type {
                    "message" => {
                        let role = payload
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let text = collect_message_text(payload, &role);
                        if text.is_empty() {
                            continue;
                        }
                        if role == "user" {
                            if let Some(open) = open_turn.as_mut() {
                                open.user_text = append_text(&open.user_text, &text);
                            } else {
                                pending_user_text = append_text(&pending_user_text, &text);
                            }
                            user_turn_slot
                                .blocks
                                .push(UserTurnBlock::text(&text, &counter));
                            if user_turn_slot.ts.is_empty() && !item_ts.is_empty() {
                                user_turn_slot.ts = item_ts.to_string();
                            }
                            if capture_content {
                                let rec = ContentRecord {
                                    v: 1,
                                    source: SourceKind::Codex,
                                    session_id: session_id.clone(),
                                    message_id: open_turn
                                        .as_ref()
                                        .map(|o| o.turn_id.clone())
                                        .unwrap_or_default(),
                                    ts: item_ts.to_string(),
                                    role: ContentRole::User,
                                    kind: ContentKind::Text,
                                    text: Some(text.clone()),
                                    tool_use: None,
                                    tool_result: None,
                                };
                                push_content(&mut open_turn, &mut pending_content, rec);
                            }
                        } else if role == "assistant" {
                            if let Some(open) = open_turn.as_mut() {
                                open.assistant_text = append_text(&open.assistant_text, &text);
                                if capture_content {
                                    open.content.push(ContentRecord {
                                        v: 1,
                                        source: SourceKind::Codex,
                                        session_id: session_id.clone(),
                                        message_id: open.turn_id.clone(),
                                        ts: item_ts.to_string(),
                                        role: ContentRole::Assistant,
                                        kind: ContentKind::Text,
                                        text: Some(text),
                                        tool_use: None,
                                        tool_result: None,
                                    });
                                }
                            }
                        }
                    }
                    "reasoning" => {
                        if !capture_content {
                            continue;
                        }
                        let Some(open) = open_turn.as_mut() else {
                            continue;
                        };
                        let text = collect_reasoning_text(payload);
                        if !text.is_empty() {
                            open.content.push(ContentRecord {
                                v: 1,
                                source: SourceKind::Codex,
                                session_id: session_id.clone(),
                                message_id: open.turn_id.clone(),
                                ts: item_ts.to_string(),
                                role: ContentRole::Assistant,
                                kind: ContentKind::Thinking,
                                text: Some(text),
                                tool_use: None,
                                tool_result: None,
                            });
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                            Some(c) => c.to_string(),
                            None => continue,
                        };
                        let output = payload.get("output").cloned().unwrap_or(Value::Null);
                        user_turn_slot.blocks.push(UserTurnBlock::tool_result(
                            call_id.clone(),
                            &output,
                            None,
                            &counter,
                        ));
                        if user_turn_slot.ts.is_empty() && !item_ts.is_empty() {
                            user_turn_slot.ts = item_ts.to_string();
                        }
                        let entry = tool_result_counters.entry(call_id.clone()).or_insert(0);
                        let call_index = *entry;
                        *entry += 1;
                        let initial_status = if open_turn
                            .as_ref()
                            .map(|o| o.errored_call_ids.contains(&call_id))
                            .unwrap_or(false)
                        {
                            ToolResultStatus::Errored
                        } else {
                            ToolResultStatus::Unknown
                        };
                        let measured = measure_tool_output(&output);
                        let mut ev = ToolResultEventRecord {
                            v: 1,
                            source: SourceKind::Codex,
                            session_id: session_id.clone(),
                            message_id: open_turn.as_ref().map(|o| o.turn_id.clone()),
                            tool_use_id: call_id.clone(),
                            call_index: Some(call_index),
                            event_index: next_event_index,
                            ts: if item_ts.is_empty() {
                                None
                            } else {
                                Some(item_ts.to_string())
                            },
                            status: initial_status,
                            event_source: ToolResultEventSource::FunctionCallOutput,
                            content_length: measured.length,
                            content_hash: measured.hash,
                            is_error: matches!(initial_status, ToolResultStatus::Errored)
                                .then_some(true),
                            usage: None,
                            usage_attribution: None,
                            subagent_session_id: None,
                            agent_id: None,
                            replaced_tools: None,
                            collapsed_calls: None,
                        };
                        next_event_index += 1;
                        if let Some(open) = open_turn.as_mut() {
                            if let Some(spawn) = open.spawn_calls.get_mut(&call_id) {
                                if let Some(sid) = extract_spawned_agent_id(&output) {
                                    spawn.spawned_agent_id = Some(sid.clone());
                                    ev.agent_id = Some(sid.clone());
                                    ev.subagent_session_id = Some(sid);
                                }
                                let info = spawn.clone();
                                maybe_emit_spawn_relationship(open, &session_id, &info, item_ts);
                            }
                            open.pending_tool_result_events.push(ev);
                        } else {
                            pending_tool_result_events.push(Pending {
                                offset: line_end_offset,
                                record: ev,
                            });
                        }
                        if capture_content {
                            let rec = ContentRecord {
                                v: 1,
                                source: SourceKind::Codex,
                                session_id: session_id.clone(),
                                message_id: open_turn
                                    .as_ref()
                                    .map(|o| o.turn_id.clone())
                                    .unwrap_or_default(),
                                ts: item_ts.to_string(),
                                role: ContentRole::ToolResult,
                                kind: ContentKind::ToolResult,
                                text: None,
                                tool_use: None,
                                tool_result: Some(ContentToolResult {
                                    tool_use_id: call_id,
                                    content: output,
                                    is_error: None,
                                }),
                            };
                            push_content(&mut open_turn, &mut pending_content, rec);
                        }
                    }
                    "function_call" => {
                        let Some(open) = open_turn.as_mut() else {
                            continue;
                        };
                        let name = match payload.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n.to_string(),
                            None => continue,
                        };
                        let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                            Some(c) => c.to_string(),
                            None => continue,
                        };
                        if open.seen_call_ids.contains(&call_id) {
                            continue;
                        }
                        open.seen_call_ids.insert(call_id.clone());
                        let arg_str = payload.get("arguments").and_then(|v| v.as_str());
                        let parsed_args = arg_str.and_then(safe_parse_json_object);
                        let hash_input = parsed_args
                            .clone()
                            .map(Value::Object)
                            .unwrap_or_else(|| Value::Object(Default::default()));
                        let target = pick_function_call_target(&name, parsed_args.as_ref());
                        let call = ToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            target,
                            args_hash: args_hash(&hash_input),
                            is_error: None,
                            edit_pre_hash: None,
                            edit_post_hash: None,
                            skill_name: None,
                            replaced_tools: None,
                            collapsed_calls: None,
                        };
                        open.tool_calls.push(call);
                        if name == "spawn_agent" {
                            let mut info = SpawnCallInfo {
                                call_id: call_id.clone(),
                                ts: item_ts.to_string(),
                                subagent_type: None,
                                description: None,
                                spawned_agent_id: None,
                                emitted: false,
                            };
                            if let Some(args) = parsed_args.as_ref() {
                                let v = Value::Object(args.clone());
                                info.subagent_type =
                                    pick_string_field(&v, &["subagent_type", "agent_type", "type"]);
                                info.description =
                                    pick_string_field(&v, &["description", "task", "prompt"]);
                                info.spawned_agent_id = pick_string_field(
                                    &v,
                                    &["agent_id", "subagent_id", "session_id"],
                                );
                            }
                            open.spawn_calls.insert(call_id.clone(), info.clone());
                            maybe_emit_spawn_relationship(open, &session_id, &info, item_ts);
                        }
                        if capture_content {
                            let input = parsed_args
                                .map(|m| m.into_iter().collect())
                                .unwrap_or_default();
                            open.content.push(ContentRecord {
                                v: 1,
                                source: SourceKind::Codex,
                                session_id: session_id.clone(),
                                message_id: open.turn_id.clone(),
                                ts: item_ts.to_string(),
                                role: ContentRole::Assistant,
                                kind: ContentKind::ToolUse,
                                text: None,
                                tool_use: Some(ContentToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                }),
                                tool_result: None,
                            });
                        }
                    }
                    "custom_tool_call" => {
                        let Some(open) = open_turn.as_mut() else {
                            continue;
                        };
                        let name = match payload.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n.to_string(),
                            None => continue,
                        };
                        let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                            Some(c) => c.to_string(),
                            None => continue,
                        };
                        if open.seen_call_ids.contains(&call_id) {
                            continue;
                        }
                        open.seen_call_ids.insert(call_id.clone());
                        let input = payload
                            .get("input")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let hash_input = serde_json::json!({ "input": input });
                        let target = pick_custom_tool_target(&name, &input);
                        let call = ToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            target,
                            args_hash: args_hash(&hash_input),
                            is_error: None,
                            edit_pre_hash: None,
                            edit_post_hash: None,
                            skill_name: None,
                            replaced_tools: None,
                            collapsed_calls: None,
                        };
                        open.tool_calls.push(call);
                        if capture_content {
                            let mut input_map = BTreeMap::new();
                            input_map.insert("input".to_string(), Value::String(input));
                            open.content.push(ContentRecord {
                                v: 1,
                                source: SourceKind::Codex,
                                session_id: session_id.clone(),
                                message_id: open.turn_id.clone(),
                                ts: item_ts.to_string(),
                                role: ContentRole::Assistant,
                                kind: ContentKind::ToolUse,
                                text: None,
                                tool_use: Some(ContentToolUse {
                                    id: call_id,
                                    name,
                                    input: input_map,
                                }),
                                tool_result: None,
                            });
                        }
                    }
                    _ => {}
                }
                continue;
            }
            _ => continue,
        }
    }

    // Emit only committed turns.
    let committed = &finalized[..committed_finalized_count];
    let mut turns: Vec<TurnRecord> = Vec::with_capacity(committed.len());
    let mut content_out: Vec<ContentRecord> = Vec::new();
    for (i, f) in committed.iter().enumerate() {
        let mut record = TurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: committed_session_id.clone(),
            session_path: options.session_path.clone(),
            message_id: f.turn_id.clone(),
            turn_index: i as u64,
            ts: f.ts.clone(),
            model: f.model.clone(),
            project: None,
            project_key: None,
            usage: f.usage.clone(),
            tool_calls: f.tool_calls.clone(),
            files_touched: if f.files_touched.is_empty() {
                None
            } else {
                Some(f.files_touched.clone())
            },
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: Some(f.fidelity.clone()),
        };
        if let Some(p) = f.project.as_ref() {
            let resolved = project_resolver.resolve(p);
            record.project = Some(resolved.project);
            record.project_key = resolved.project_key;
        }
        let combined_text = join_nonempty(&[f.user_text.as_str(), f.assistant_text.as_str()], "\n");
        let has_failed_tool = f
            .tool_calls
            .iter()
            .any(|tc| f.errored_call_ids.contains(&tc.id));
        let classified = classify_activity(ClassificationInput {
            tool_calls: &f.tool_calls,
            text: &combined_text,
            has_failed_tool,
            reasoning_tokens: f.usage.reasoning,
        });
        record.activity = Some(classified.activity);
        record.retries = Some(classified.retries);
        record.has_edits = Some(classified.has_edits);
        turns.push(record);
        if capture_content {
            content_out.extend(f.content.clone());
        }
    }

    let resume = CodexResumeState {
        cumulative: committed_cumulative.clone(),
        session_id: committed_session_id.clone(),
        session_cwd: committed_session_cwd.clone(),
        turn_contexts: committed_turn_contexts.clone(),
        user_turn_slot: Some(committed_user_turn_slot.to_persisted()),
        root_session_emitted: committed_root_session_emitted,
        session_meta_relationship_keys: committed_seen_session_meta_keys.iter().cloned().collect(),
        next_event_index: committed_next_event_index,
        tool_result_counters: committed_tool_result_counters.clone(),
        last_completed_turn: committed_last_completed_turn.clone(),
    };

    let user_turns_out = user_turns[..committed_user_turns_count].to_vec();
    let mut events_out: Vec<CompactionEvent> = Vec::new();
    for e in pending_compactions {
        if e.offset <= committed_end_offset {
            events_out.push(e.record);
        }
    }
    let mut relationships_out: Vec<SessionRelationshipRecord> = Vec::new();
    for r in pending_relationships {
        if r.offset <= committed_end_offset {
            relationships_out.push(r.record);
        }
    }
    let mut tool_events_out: Vec<ToolResultEventRecord> = Vec::new();
    for ev in pending_tool_result_events {
        if ev.offset <= committed_end_offset {
            tool_events_out.push(ev.record);
        }
    }

    Ok(ParseCodexIncrementalResult {
        turns,
        content: content_out,
        events: events_out,
        user_turns: user_turns_out,
        relationships: relationships_out,
        tool_result_events: tool_events_out,
        end_offset: committed_end_offset,
        resume,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolves the requested tokenizer to a concrete counter. `None` and
/// `Some(Heuristic)` map to [`HeuristicCounter`]; `Some(Cl100k)` is rejected
/// with an explicit error until the cl100k counter is wired up (see #246) so
/// callers don't silently get bytes/4 sizing when they asked for cl100k.
fn resolve_token_counter(
    tokenizer: Option<UserTurnTokenizer>,
) -> std::io::Result<HeuristicCounter> {
    match tokenizer {
        None | Some(UserTurnTokenizer::Heuristic) => Ok(HeuristicCounter),
        Some(UserTurnTokenizer::Cl100k) => Err(std::io::Error::other(
            "cl100k tokenizer is not yet available in the Rust port; \
             omit `tokenizer` or pass `Some(Heuristic)` (see AgentWorkforce/burn#246)",
        )),
    }
}

fn session_meta_payload_id(payload: &Value) -> Option<String> {
    let id = payload.get("id")?.as_str()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn collect_message_text(payload: &Value, role: &str) -> String {
    let Some(content) = payload.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if role == "user" && is_codex_boilerplate(text) {
            continue;
        }
        parts.push(text.to_string());
    }
    parts.join("\n")
}

fn is_codex_boilerplate(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lower_first16: String = trimmed.chars().take(32).collect::<String>().to_lowercase();
    if lower_first16.starts_with("<environment_context")
        || lower_first16.starts_with("<permissions")
        || lower_first16.starts_with("<collaboration_mode")
    {
        return true;
    }
    if trimmed.starts_with("<INSTRUCTIONS>") {
        return true;
    }
    // `^\s*#\s*AGENTS\.md`/i — case-insensitive.
    let after_hash = trimmed.strip_prefix('#').map(str::trim_start);
    if let Some(rest) = after_hash {
        let rest_lower: String = rest.chars().take(16).collect::<String>().to_lowercase();
        if rest_lower.starts_with("agents.md") {
            return true;
        }
    }
    false
}

fn collect_reasoning_text(payload: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(arr) = payload.get("summary").and_then(|v| v.as_array()) {
        for s in arr {
            if let Some(text) = s.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if let Some(arr) = payload.get("content").and_then(|v| v.as_array()) {
        for c in arr {
            if let Some(text) = c.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

fn append_text(existing: &str, next: &str) -> String {
    if existing.is_empty() {
        next.to_string()
    } else {
        format!("{}\n{}", existing, next)
    }
}

fn join_nonempty(parts: &[&str], sep: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        if !p.is_empty() {
            out.push(p);
        }
    }
    out.join(sep)
}

fn safe_parse_json_object(s: &str) -> Option<serde_json::Map<String, Value>> {
    if s.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(s).ok()?;
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn pick_function_call_target(
    name: &str,
    args: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    let args = args?;
    let s =
        |k: &str| -> Option<String> { args.get(k).and_then(|v| v.as_str()).map(|x| x.to_string()) };
    match name {
        "exec_command" | "shell" => s("cmd").or_else(|| s("command")),
        "read_file" => s("path").or_else(|| s("file_path")),
        "write_file" => s("path").or_else(|| s("file_path")),
        _ => s("path")
            .or_else(|| s("file_path"))
            .or_else(|| s("cmd"))
            .or_else(|| s("command"))
            .or_else(|| s("url")),
    }
}

fn pick_custom_tool_target(name: &str, input: &str) -> Option<String> {
    if name != "apply_patch" {
        return None;
    }
    // Match `^***\s+(?:Update|Add|Delete)\s+File:\s+(\S.*?)\s*$` per-line.
    for line in input.lines() {
        let l = line.trim_start();
        if !l.starts_with("***") {
            continue;
        }
        let rest = l.trim_start_matches('*').trim_start();
        for verb in ["Update", "Add", "Delete"] {
            if let Some(rest) = rest.strip_prefix(verb) {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix("File:") {
                    let rest = rest.trim();
                    if !rest.is_empty() {
                        return Some(rest.to_string());
                    }
                }
            }
        }
    }
    None
}

fn pick_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn extract_spawned_agent_id(output: &Value) -> Option<String> {
    let obj_owner;
    let obj = match output {
        Value::Object(_) => output,
        Value::String(s) => {
            obj_owner = serde_json::from_str::<Value>(s).ok()?;
            if !obj_owner.is_object() {
                return None;
            }
            &obj_owner
        }
        _ => return None,
    };
    pick_string_field(obj, &["agent_id", "subagent_id", "session_id"])
}

#[derive(Default)]
struct Measured {
    length: Option<u64>,
    hash: Option<String>,
}

fn measure_tool_output(output: &Value) -> Measured {
    match output {
        Value::Null => Measured::default(),
        Value::String(s) => Measured {
            length: Some(s.len() as u64),
            hash: Some(content_hash(s)),
        },
        other => match serde_json::to_string(other) {
            Ok(serialized) => Measured {
                length: Some(serialized.len() as u64),
                hash: Some(content_hash(&serialized)),
            },
            Err(_) => Measured::default(),
        },
    }
}

fn is_subagent_terminal_notification(t: &str) -> bool {
    if !t.starts_with("subagent_") {
        return false;
    }
    t.ends_with("_complete")
        || t.ends_with("_done")
        || t.ends_with("_finished")
        || t.ends_with("_terminated")
}

fn subagent_notification_status(payload: &Value) -> ToolResultStatus {
    if let Some(b) = payload.get("success").and_then(|v| v.as_bool()) {
        return if b {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Errored
        };
    }
    if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
        let s = s.to_ascii_lowercase();
        if s == "errored" || s == "failed" || s == "error" {
            return ToolResultStatus::Errored;
        }
        if s == "cancelled" || s == "canceled" {
            return ToolResultStatus::Cancelled;
        }
        if s == "completed" || s == "success" || s == "succeeded" {
            return ToolResultStatus::Completed;
        }
    }
    ToolResultStatus::Completed
}

fn build_root_relationship(session_id: &str, ts: &str, meta: &Value) -> SessionRelationshipRecord {
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Codex,
        session_id: session_id.to_string(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: if ts.is_empty() {
            None
        } else {
            Some(ts.to_string())
        },
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    apply_codex_session_meta_provenance(&mut row, meta);
    row
}

fn build_session_meta_relationships(
    session_id: &str,
    meta: &Value,
    fallback_ts: &str,
) -> Vec<SessionRelationshipRecord> {
    let mut rows = Vec::new();
    let ts = pick_string_field(meta, &["timestamp"]).unwrap_or_else(|| fallback_ts.to_string());
    if let Some(fork_id) = pick_string_field(meta, &["forkSessionId", "fork_session_id"]) {
        if fork_id != session_id {
            let mut row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::Codex,
                session_id: session_id.to_string(),
                related_session_id: Some(fork_id),
                relationship_type: RelationshipType::Fork,
                ts: if ts.is_empty() {
                    None
                } else {
                    Some(ts.clone())
                },
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            apply_codex_session_meta_provenance(&mut row, meta);
            rows.push(row);
        }
    }
    if let Some(prev_id) = pick_string_field(
        meta,
        &["continuedFromSessionId", "continued_from_session_id"],
    ) {
        if prev_id != session_id {
            let mut row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::Codex,
                session_id: session_id.to_string(),
                related_session_id: Some(prev_id),
                relationship_type: RelationshipType::Continuation,
                ts: if ts.is_empty() {
                    None
                } else {
                    Some(ts.clone())
                },
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            apply_codex_session_meta_provenance(&mut row, meta);
            rows.push(row);
        }
    }
    rows
}

fn apply_codex_session_meta_provenance(row: &mut SessionRelationshipRecord, meta: &Value) {
    if let Some(s) = pick_string_field(meta, &["sourceSessionId", "source_session_id"]) {
        row.source_session_id = Some(s);
    }
    if let Some(s) = pick_string_field(meta, &["cli_version", "version"]) {
        row.source_version = Some(s);
    }
}

fn codex_relationship_key(row: &SessionRelationshipRecord) -> String {
    let source = match row.source {
        RelationshipSourceKind::Codex => "codex",
        RelationshipSourceKind::ClaudeCode => "claude-code",
        RelationshipSourceKind::Opencode => "opencode",
        RelationshipSourceKind::AnthropicApi => "anthropic-api",
        RelationshipSourceKind::OpenaiApi => "openai-api",
        RelationshipSourceKind::GeminiApi => "gemini-api",
        RelationshipSourceKind::SpawnEnv => "spawn-env",
        RelationshipSourceKind::NativeClaude => "native-claude",
        RelationshipSourceKind::NativeOpencode => "native-opencode",
    };
    let rel = match row.relationship_type {
        RelationshipType::Root => "root",
        RelationshipType::Continuation => "continuation",
        RelationshipType::Fork => "fork",
        RelationshipType::Subagent => "subagent",
    };
    format!(
        "{}|{}|{}|{}|{}|{}",
        source,
        row.session_id,
        rel,
        row.related_session_id.as_deref().unwrap_or(""),
        row.agent_id.as_deref().unwrap_or(""),
        row.parent_tool_use_id.as_deref().unwrap_or(""),
    )
}

fn maybe_emit_spawn_relationship(
    open_turn: &mut OpenTurn,
    session_id: &str,
    info: &SpawnCallInfo,
    ts: &str,
) {
    let already = match open_turn.spawn_calls.get(&info.call_id) {
        Some(s) => s.emitted,
        None => false,
    };
    if already {
        return;
    }
    let Some(spawned_id) = info.spawned_agent_id.as_ref() else {
        return;
    };
    let stamp = if ts.is_empty() {
        info.ts.clone()
    } else {
        ts.to_string()
    };
    let row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Codex,
        session_id: spawned_id.clone(),
        related_session_id: Some(session_id.to_string()),
        relationship_type: RelationshipType::Subagent,
        ts: if stamp.is_empty() { None } else { Some(stamp) },
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: Some(info.call_id.clone()),
        agent_id: Some(spawned_id.clone()),
        subagent_type: info.subagent_type.clone(),
        description: info.description.clone(),
    };
    open_turn.pending_relationships.push(row);
    if let Some(s) = open_turn.spawn_calls.get_mut(&info.call_id) {
        s.emitted = true;
    }
}

fn push_content(
    open_turn: &mut Option<OpenTurn>,
    pending: &mut Vec<ContentRecord>,
    record: ContentRecord,
) {
    if let Some(open) = open_turn.as_mut() {
        open.content.push(record);
    } else {
        pending.push(record);
    }
}

fn build_codex_user_turn_record(
    slot: &UserTurnSlot,
    session_id: &str,
    following_message_id: &str,
    fallback_ts: &str,
) -> UserTurnRecord {
    let preceding_tag = slot.preceding_message_id.as_deref().unwrap_or("start");
    let user_uuid = format!("{}:{}->{}", session_id, preceding_tag, following_message_id);
    let ts = if !slot.ts.is_empty() {
        slot.ts.clone()
    } else {
        fallback_ts.to_string()
    };
    UserTurnRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: session_id.to_string(),
        user_uuid,
        ts,
        preceding_message_id: slot.preceding_message_id.clone(),
        following_message_id: Some(following_message_id.to_string()),
        blocks: slot.blocks.clone(),
    }
}

fn build_codex_compaction_event(
    session_id: &str,
    ts: &str,
    preceding: Option<&CodexLastCompletedTurn>,
) -> CompactionEvent {
    let mut event = CompactionEvent {
        v: 1,
        source: SourceKind::Codex,
        session_id: session_id.to_string(),
        ts: ts.to_string(),
        preceding_message_id: None,
        tokens_before_compact: None,
    };
    if let Some(prev) = preceding {
        event.preceding_message_id = Some(prev.message_id.clone());
        event.tokens_before_compact = Some(prev.cache_read);
    }
    event
}

fn clone_resume(r: Option<&CodexResumeState>) -> CodexResumeState {
    match r {
        None => CodexResumeState {
            user_turn_slot: Some(PersistedUserTurnSlot::default()),
            ..Default::default()
        },
        Some(r) => CodexResumeState {
            cumulative: r.cumulative.clone(),
            session_id: r.session_id.clone(),
            session_cwd: r.session_cwd.clone(),
            turn_contexts: r.turn_contexts.clone(),
            user_turn_slot: r
                .user_turn_slot
                .clone()
                .or_else(|| Some(PersistedUserTurnSlot::default())),
            root_session_emitted: r.root_session_emitted,
            session_meta_relationship_keys: r.session_meta_relationship_keys.clone(),
            next_event_index: r.next_event_index,
            tool_result_counters: r.tool_result_counters.clone(),
            last_completed_turn: r.last_completed_turn.clone(),
        },
    }
}

#[cfg(test)]
mod tests;
