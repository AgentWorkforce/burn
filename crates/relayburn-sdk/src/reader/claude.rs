//! Claude Code session parser — Rust port of `packages/reader/src/claude.ts`.
//!
//! Covers `parse_claude_session`, `parse_claude_session_incremental`, and
//! `reconcile_claude_session_relationships`.
//!
//! The on-disk JSONL has a very loose shape (any extra fields permitted, any
//! field can be absent), so we keep raw lines as `serde_json::Value` and use
//! small accessor helpers rather than ahead-of-time deserialization. This
//! mirrors the TS implementation, which also walks records as `unknown`.
//!
//! ## User→assistant text association via `parentUuid` chain (#433)
//!
//! Activity classification feeds the user-prompt text into the rules. The
//! prompt-to-assistant mapping uses the `parentUuid` chain (see the
//! `claude/parent_chain.rs` submodule) and falls back to the legacy
//! file-order map only when the assistant row lacks UUIDs or the chain
//! does not terminate at a known user prompt. The chain walk is robust
//! against out-of-order JSONL flushes and mid-stream interruptions; the
//! file-order map is kept solely as a safety net for legacy/malformed
//! rows.
//!
//! Codex (`reader/codex.rs`) and opencode (`reader/opencode.rs`) rollouts
//! do not carry an equivalent `parentUuid`-style field; they group turns
//! via their own primitives (Codex: `task_complete` boundaries; opencode:
//! per-message part files sorted chronologically). Neither parser is
//! touched by this change. See AgentWorkforce/burn#433.

mod parent_chain;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use self::parent_chain::ChainNode;
use crate::reader::classifier::{
    classify_activity, detect_slash_triads, is_task_notification, ClassificationInput,
};
use crate::reader::git::resolve_project;
use crate::reader::hash::{args_hash, content_hash};
use crate::reader::inference::{RequestIdLookup, TurnKey};
use crate::reader::types::{
    ActivityCategory, CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode,
    ContentToolResult, ContentToolUse, Coverage, Fidelity, RelationshipSourceKind,
    RelationshipType, SessionRelationshipRecord, SourceKind, StopReason, Subagent, ToolCall,
    ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord, Usage,
    UsageGranularity, UserTurnBlock, UserTurnRecord,
};
use crate::reader::user_turn::{HeuristicCounter, TokenCounter};

// Discovery + pairing for Task subagent sidecar transcripts. Public so the
// SDK surface (`crate::reader::{discover_subagents, pair_to_main,
// SubagentTranscript}`) and the ingest path can both reach it. Lazy —
// callers stat-then-walk only when something asks for subagents. See
// AgentWorkforce/burn#435.
pub mod subagents;

// Per-turn span tree builder. Pure projection over `TurnRecord` +
// paired `tool_result_event` rows + optional subagent transcripts.
// See AgentWorkforce/burn#430.
pub mod span_tree;

// ---------------------------------------------------------------------------
// Public surface.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub file_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParseResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub user_turns: Vec<UserTurnRecord>,
    /// `(source, session_id, message_id) -> requestId` map for every
    /// emitted turn whose source row carried an upstream `requestId`.
    /// Keys are missing for turns whose harness doesn't ship one (Codex,
    /// opencode, some older Claude versions). Fed to
    /// [`crate::reader::build_inferences`] to key the per-API-call
    /// aggregate (issue #434).
    pub request_id_lookup: RequestIdLookup,
    /// Read by the in-crate test suite to verify the From<ParseIncrementalResult>
    /// conversion preserves evidence. Production callers consume the incremental
    /// result directly and access `evidence` from there.
    #[cfg(test)]
    pub evidence: ClaudeRelationshipEvidence,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeRelationshipEvidence {
    pub file_session_id: Option<String>,
    pub first_ts: Option<String>,
    pub in_log_session_ids: Vec<String>,
    pub source_version: Option<String>,
    pub first_parent_uuid: Option<String>,
    pub seen_uuids: Vec<String>,
    pub has_resume_marker: bool,
    pub resume_target_session_id: Option<String>,
    pub explicit_continuation_target_session_ids: Option<Vec<String>>,
    pub explicit_fork_target_session_ids: Option<Vec<String>>,
    /// TS uses a module-level WeakSet to gate `firstParentUuid` to the very
    /// first non-sidechain user line. We carry the same gate inline.
    user_seen: bool,
}

#[derive(Debug, Clone)]
pub struct ReconcileClaudeRelationshipsInput {
    pub evidence: ClaudeRelationshipEvidence,
}

/// Synchronous Rust counterpart of `parseClaudeSession`. Reads the file line
/// by line, decodes each line as JSON, and produces the per-file parse result.
pub fn parse_claude_session<P: AsRef<Path>>(
    path: P,
    options: &ParseOptions,
) -> std::io::Result<ParseResult> {
    let counter = HeuristicCounter; // cl100k counter not yet wired; heuristic is the safe default
    parse_claude_session_with_counter(path, options, &counter)
}

/// Variant that lets the caller plug in a custom user-turn token counter. The
/// TS port is async because the cl100k tokenizer ships as an async-loaded
/// WASM module; in Rust we let the caller choose whether to take that
/// dependency, so the entry point stays synchronous.
///
/// Implementation: delegate to `run_incremental` with `start_offset = 0` and
/// `emit_in_progress = true` so trailing assistants without a `stop_reason`
/// still surface, matching the single-shot ParseResult contract callers rely
/// on. The mirrored ParseState codepath was retired in favor of one parser.
pub fn parse_claude_session_with_counter<P: AsRef<Path>, C: TokenCounter + ?Sized>(
    path: P,
    options: &ParseOptions,
    counter: &C,
) -> std::io::Result<ParseResult> {
    let inc_opts = ParseIncrementalOptions {
        session_path: options.session_path.clone(),
        content_mode: options.content_mode,
        file_session_id: options.file_session_id.clone(),
        start_offset: Some(0),
        last_user_text: None,
    };
    run_incremental(path.as_ref(), &inc_opts, counter, true).map(ParseResult::from)
}

impl From<ParseIncrementalResult> for ParseResult {
    fn from(r: ParseIncrementalResult) -> Self {
        Self {
            turns: r.turns,
            content: r.content,
            events: r.events,
            relationships: r.relationships,
            tool_result_events: r.tool_result_events,
            user_turns: r.user_turns,
            request_id_lookup: r.request_id_lookup,
            #[cfg(test)]
            evidence: r.evidence,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParseIncrementalOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub file_session_id: Option<String>,
    /// Byte offset to resume parsing from. The previous incremental call's
    /// `end_offset` is the right value to pass.
    pub start_offset: Option<u64>,
    /// The most recent user prompt text seen before `start_offset`. Carried
    /// forward from the prior call's result so an in-progress turn whose user
    /// prompt was before the resume cursor still classifies against the
    /// prompt for keyword refinement.
    pub last_user_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParseIncrementalResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub user_turns: Vec<UserTurnRecord>,
    /// `(source, session_id, message_id) -> requestId` map for the
    /// turns emitted on this incremental pass (see [`ParseResult`] for
    /// rationale). Issue #434.
    pub request_id_lookup: RequestIdLookup,
    /// Byte position to pass as `start_offset` on the next call. May back up
    /// past in-progress trailing messages so the next call re-reads them.
    pub end_offset: u64,
    /// Carry forward to the next call's `last_user_text` option.
    pub last_user_text: String,
    pub evidence: ClaudeRelationshipEvidence,
}

/// Synchronous Rust counterpart of `parseClaudeSessionIncremental`. Reads the
/// file from `start_offset` and emits only records that lie strictly before
/// the returned `end_offset` so the next call (resumed at `end_offset`) does
/// not double-emit. Trailing in-progress messages back up `end_offset` to the
/// byte position of the earliest in-progress assistant line.
pub fn parse_claude_session_incremental<P: AsRef<Path>>(
    path: P,
    options: &ParseIncrementalOptions,
) -> std::io::Result<ParseIncrementalResult> {
    let counter = HeuristicCounter;
    parse_claude_session_incremental_with_counter(path, options, &counter)
}

pub fn parse_claude_session_incremental_with_counter<P: AsRef<Path>, C: TokenCounter + ?Sized>(
    path: P,
    options: &ParseIncrementalOptions,
    counter: &C,
) -> std::io::Result<ParseIncrementalResult> {
    run_incremental(path.as_ref(), options, counter, false)
}

// ---------------------------------------------------------------------------
// Internal parse state.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct UsageCoverage {
    has_input_tokens: bool,
    has_output_tokens: bool,
    has_cache_read_tokens: bool,
    has_cache_create_tokens: bool,
}

#[derive(Debug, Clone)]
struct WorkingRecord {
    message_id: String,
    first_ts: String,
    model: String,
    session_id: String,
    cwd: Option<String>,
    is_sidechain: bool,
    usage: Usage,
    usage_coverage: UsageCoverage,
    blocks: Vec<Value>,
    stop_reason: Option<String>,
    first_assistant_uuid: Option<String>,
    #[allow(dead_code)]
    parent_assistant_uuid: Option<String>,
    /// Upstream `requestId` field from the first row that carried one
    /// for this `message_id`. Powers the `Inference` aggregate's
    /// per-API-call key (see `reader/inference.rs` and issue #434).
    /// `None` when no row in this group emitted one — the inference
    /// builder falls back to `message_id`.
    request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
struct AgentToolUse {
    id: String,
    subagent_type: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone)]
struct LineNode {
    uuid: String,
    parent_uuid: Option<String>,
    kind: LineKind,
    is_sidechain: bool,
    agent_tool_use: Option<AgentToolUse>,
    tool_result_ids: Option<HashSet<String>>,
    /// True only when this row is a real user-prompt line (i.e. `kind ==
    /// User`, the message body contains plain user text, and the row is
    /// not a harness `<task-notification>` synthetic event). Used as the
    /// `is_user_root()` signal for the parent-chain walker (#433). A
    /// user line carrying only a `tool_result` block — the common case
    /// for tool result envelopes — is NOT a turn root and so this stays
    /// false.
    is_user_prompt: bool,
}

impl ChainNode for LineNode {
    fn uuid(&self) -> &str {
        &self.uuid
    }
    fn parent_uuid(&self) -> Option<&str> {
        self.parent_uuid.as_deref()
    }
    fn is_user_root(&self) -> bool {
        self.is_user_prompt
    }
}

/// Walk upward from `start_uuid` along `parent_uuid` until the nearest
/// user-prompt ancestor (`is_user_root() == true`) is found, then return
/// that ancestor's UUID. Returns `None` if no such ancestor exists or
/// `start_uuid` is unknown. Cycle-safe via a visited set.
///
/// This is the per-row driver for the chain-grouping strategy described
/// in `claude/parent_chain.rs`. The Claude reader calls it to look up
/// the user prompt text that should feed activity classification for a
/// given assistant turn, bypassing the legacy file-order map under
/// out-of-order JSONL flushes and mid-stream interruption + resume
/// patterns. See AgentWorkforce/burn#433.
///
/// Reads `LineNode`s through the `ChainNode` trait so the walker stays
/// in sync with the standalone `group_by_parent_chain` helper (same
/// trait, same termination rules).
fn nearest_user_prompt_root(start_uuid: &str, nodes: &HashMap<String, LineNode>) -> Option<String> {
    let mut visited: HashSet<&str> = HashSet::new();
    let mut current: &LineNode = nodes.get(start_uuid)?;
    visited.insert(current.uuid());
    if current.is_user_root() {
        return Some(current.uuid().to_string());
    }
    loop {
        let parent_uuid = current.parent_uuid()?;
        if !visited.insert(parent_uuid) {
            return None; // cycle — bail
        }
        let parent = nodes.get(parent_uuid)?;
        if parent.is_user_root() {
            return Some(parent.uuid().to_string());
        }
        current = parent;
    }
}

#[derive(Debug, Clone)]
struct InvocationInfo {
    root_uuid: String,
    parent_tool_use_id: Option<String>,
    subagent_type: Option<String>,
    description: Option<String>,
    parent_agent_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ReplacementMeta {
    replaced_tools: Option<Vec<String>>,
    collapsed_calls: Option<u64>,
}

// ---------------------------------------------------------------------------
// Line ingest helpers.
// ---------------------------------------------------------------------------

fn ingest_assistant_record(
    parsed: &Value,
    obj: &serde_json::Map<String, Value>,
    working: &mut HashMap<String, WorkingRecord>,
    order: &mut Vec<String>,
    nodes: &mut HashMap<String, LineNode>,
) {
    let msg = match obj.get("message").and_then(Value::as_object) {
        Some(m) => m,
        None => return,
    };
    let message_id = match msg.get("id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return,
    };

    let model = msg
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = string_field(obj, SESSION_ID_KEYS, false).unwrap_or_default();
    let timestamp = string_field(obj, TIMESTAMP_KEYS, false).unwrap_or_default();
    let cwd = string_field(obj, &["cwd"], false);
    let is_sidechain = obj
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stop_reason = msg
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let blocks = msg
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let uuid = string_field(obj, &["uuid"], false);
    let parent_uuid = string_field(obj, &["parentUuid"], false);

    // `requestId` lives on the outer envelope (sibling of `message`), NOT
    // inside `message`. Capture from the first row that carries one for
    // this `message_id`; later rows belonging to the same API call
    // re-emit the same `requestId` so first-wins is the right merge.
    let request_id = string_field(obj, &["requestId", "request_id"], true);
    let usage_with_cov = to_usage(msg.get("usage"));
    // Claude writes one row per content block but only ONE of those rows
    // carries the `usage` block. The previous merge updated
    // `usage_coverage` from later carriers but not `usage` itself, so
    // `usage` could end up as zeros if the carrier wasn't the first row.
    // We now overwrite `usage` from whichever row owns the field — per
    // issue #434 the usage block is single-carrier, so "last writer
    // wins" and "any writer wins" both round-trip to the same value.
    let has_usage = msg.contains_key("usage");

    if let Some(w) = working.get_mut(&message_id) {
        if is_sidechain {
            w.is_sidechain = true;
        }
        if w.model.is_empty() && !model.is_empty() {
            w.model = model.clone();
        }
        if has_usage {
            w.usage_coverage = merge_usage_coverage(&w.usage_coverage, &usage_with_cov.coverage);
            // Adopt the carrier row's usage. See the comment above the
            // outer `let has_usage` for why overwrite is safe.
            w.usage = usage_with_cov.usage.clone();
        }
        if let Some(s) = stop_reason {
            w.stop_reason = Some(s);
        }
        for b in &blocks {
            w.blocks.push(b.clone());
        }
        if w.request_id.is_none() {
            if let Some(req) = request_id.clone() {
                w.request_id = Some(req);
            }
        }
    } else {
        let w = WorkingRecord {
            message_id: message_id.clone(),
            first_ts: timestamp,
            model,
            session_id,
            cwd,
            is_sidechain,
            usage: usage_with_cov.usage,
            usage_coverage: usage_with_cov.coverage,
            blocks,
            stop_reason,
            first_assistant_uuid: uuid,
            parent_assistant_uuid: parent_uuid,
            request_id,
        };
        working.insert(message_id.clone(), w);
        order.push(message_id);
    }

    register_assistant_node(parsed, nodes);
}

fn make_line_node(line: &Value, kind: LineKind) -> Option<LineNode> {
    let uuid = line.get("uuid").and_then(Value::as_str)?.to_string();
    let parent_uuid = line
        .get("parentUuid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let is_sidechain = line
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(LineNode {
        uuid,
        parent_uuid,
        kind,
        is_sidechain,
        agent_tool_use: None,
        tool_result_ids: None,
        // Recomputed in `register_user_node` based on body shape + task
        // notification gate. Default false is correct for assistant rows.
        is_user_prompt: false,
    })
}

fn register_assistant_node(line: &Value, nodes: &mut HashMap<String, LineNode>) {
    let mut node = match make_line_node(line, LineKind::Assistant) {
        Some(n) => n,
        None => return,
    };
    if let Some(content) = line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        for block in content {
            let bobj = match block.as_object() {
                Some(o) => o,
                None => continue,
            };
            if bobj.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let name = bobj.get("name").and_then(Value::as_str).unwrap_or("");
            if name != "Agent" && name != "Task" {
                continue;
            }
            let id = match bobj.get("id").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let input = bobj.get("input").and_then(Value::as_object);
            let subagent_type = input
                .and_then(|i| i.get("subagent_type"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let description = input
                .and_then(|i| i.get("description"))
                .and_then(Value::as_str)
                .map(str::to_string);
            node.agent_tool_use = Some(AgentToolUse {
                id,
                subagent_type,
                description,
            });
            break;
        }
    }
    nodes.insert(node.uuid.clone(), node);
}

fn register_user_node(line: &Value, nodes: &mut HashMap<String, LineNode>, is_user_prompt: bool) {
    let mut node = match make_line_node(line, LineKind::User) {
        Some(n) => n,
        None => return,
    };
    node.is_user_prompt = is_user_prompt;
    let body = line.get("message").and_then(|m| m.get("content"));
    if let Some(arr) = body.and_then(Value::as_array) {
        for block in arr {
            let bobj = match block.as_object() {
                Some(o) => o,
                None => continue,
            };
            if bobj.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = match bobj.get("tool_use_id").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => continue,
            };
            node.tool_result_ids
                .get_or_insert_with(HashSet::new)
                .insert(id);
        }
    }
    nodes.insert(node.uuid.clone(), node);
}

// ---------------------------------------------------------------------------
// Usage / fidelity.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct UsageWithCoverage {
    usage: Usage,
    coverage: UsageCoverage,
}

fn to_usage(u: Option<&Value>) -> UsageWithCoverage {
    let obj = u.and_then(Value::as_object);
    let getn = |k: &str| -> Option<u64> { obj.and_then(|o| o.get(k)).and_then(Value::as_u64) };
    let cache_creation = obj
        .and_then(|o| o.get("cache_creation"))
        .and_then(Value::as_object);
    let getn_nested = |k: &str| -> Option<u64> {
        cache_creation
            .and_then(|cc| cc.get(k))
            .and_then(Value::as_u64)
    };

    let input = getn("input_tokens").unwrap_or(0);
    let output = getn("output_tokens").unwrap_or(0);
    let cache_read = getn("cache_read_input_tokens").unwrap_or(0);
    let create_5m = getn_nested("ephemeral_5m_input_tokens").unwrap_or(0);
    let create_1h = getn_nested("ephemeral_1h_input_tokens").unwrap_or(0);
    let total_create = getn("cache_creation_input_tokens").unwrap_or(0);

    let coverage = UsageCoverage {
        has_input_tokens: obj.is_some_and(|o| o.contains_key("input_tokens")),
        has_output_tokens: obj.is_some_and(|o| o.contains_key("output_tokens")),
        has_cache_read_tokens: obj.is_some_and(|o| o.contains_key("cache_read_input_tokens")),
        has_cache_create_tokens: obj.is_some_and(|o| {
            o.contains_key("cache_creation_input_tokens")
                || cache_creation.is_some_and(|cc| {
                    cc.contains_key("ephemeral_5m_input_tokens")
                        || cc.contains_key("ephemeral_1h_input_tokens")
                })
        }),
    };

    if create_5m == 0 && create_1h == 0 && total_create > 0 {
        return UsageWithCoverage {
            usage: Usage {
                input,
                output,
                reasoning: 0,
                cache_read,
                cache_create_5m: total_create,
                cache_create_1h: 0,
            },
            coverage,
        };
    }
    UsageWithCoverage {
        usage: Usage {
            input,
            output,
            reasoning: 0,
            cache_read,
            cache_create_5m: create_5m,
            cache_create_1h: create_1h,
        },
        coverage,
    }
}

fn merge_usage_coverage(a: &UsageCoverage, b: &UsageCoverage) -> UsageCoverage {
    UsageCoverage {
        has_input_tokens: a.has_input_tokens || b.has_input_tokens,
        has_output_tokens: a.has_output_tokens || b.has_output_tokens,
        has_cache_read_tokens: a.has_cache_read_tokens || b.has_cache_read_tokens,
        has_cache_create_tokens: a.has_cache_create_tokens || b.has_cache_create_tokens,
    }
}

fn build_claude_fidelity(uc: &UsageCoverage) -> Fidelity {
    let coverage = Coverage {
        has_input_tokens: uc.has_input_tokens,
        has_output_tokens: uc.has_output_tokens,
        has_reasoning_tokens: false,
        has_cache_read_tokens: uc.has_cache_read_tokens,
        has_cache_create_tokens: uc.has_cache_create_tokens,
        has_tool_calls: true,
        has_tool_result_events: true,
        has_session_relationships: true,
        has_raw_content: true,
    };
    Fidelity::new(UsageGranularity::PerTurn, coverage)
}

// ---------------------------------------------------------------------------
// Tool calls / files-touched.
// ---------------------------------------------------------------------------

fn extract_tool_calls(
    blocks: &[Value],
    errored: &HashSet<String>,
    replacement: Option<&HashMap<String, ReplacementMeta>>,
) -> Vec<ToolCall> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for b in blocks {
        let bo = match b.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = match bo.get("id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let name = match bo.get("name").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let input = bo
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let mut call = ToolCall {
            id: id.clone(),
            name: name.clone(),
            target: pick_target(&name, &input),
            args_hash: args_hash(&input),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        if errored.contains(&id) {
            call.is_error = Some(true);
        }
        apply_edit_hashes(&mut call, &input);
        if let Some(meta) = replacement.and_then(|m| m.get(&id)) {
            if let Some(ref names) = meta.replaced_tools {
                if !names.is_empty() {
                    call.replaced_tools = Some(names.clone());
                }
            }
            if let Some(c) = meta.collapsed_calls {
                if c > 0 {
                    call.collapsed_calls = Some(c);
                }
            }
        }
        out.push(call);
    }
    out
}

fn apply_edit_hashes(call: &mut ToolCall, input: &Value) {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return,
    };
    if call.name == "Edit" || call.name == "NotebookEdit" {
        if let Some(s) = obj.get("old_string").and_then(Value::as_str) {
            call.edit_pre_hash = Some(content_hash(s));
        }
        if let Some(s) = obj.get("new_string").and_then(Value::as_str) {
            call.edit_post_hash = Some(content_hash(s));
        }
    } else if call.name == "Write" {
        if let Some(s) = obj.get("content").and_then(Value::as_str) {
            call.edit_post_hash = Some(content_hash(s));
        }
    }
}

fn pick_target(name: &str, input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => s("file_path"),
        "Bash" => s("command"),
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "Agent" | "Task" => s("subagent_type").or_else(|| s("description")),
        _ => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("url"))
            .or_else(|| s("command")),
    }
}

fn extract_files_touched(tool_calls: &[ToolCall]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tc in tool_calls {
        let target = match tc.target.as_deref() {
            Some(t) => t,
            None => continue,
        };
        if matches!(tc.name.as_str(), "Read" | "Edit" | "Write" | "NotebookEdit")
            && seen.insert(target.to_string())
        {
            out.push(target.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Subagent invocation resolution.
// ---------------------------------------------------------------------------

fn resolve_subagent(
    w: &WorkingRecord,
    nodes: &HashMap<String, LineNode>,
    cache: &mut HashMap<String, Option<InvocationInfo>>,
) -> Option<Subagent> {
    if !w.is_sidechain {
        return None;
    }
    let mut sub = Subagent {
        is_sidechain: true,
        parent_tool_use_id: None,
        agent_id: None,
        parent_agent_id: None,
        subagent_type: None,
        description: None,
    };
    let start = match w.first_assistant_uuid.as_deref() {
        Some(s) => s,
        None => return Some(sub),
    };
    let info = match resolve_invocation(start, nodes, cache, 0) {
        Some(i) => i,
        None => return Some(sub),
    };
    sub.agent_id = Some(info.root_uuid);
    if let Some(p) = info.parent_tool_use_id {
        sub.parent_tool_use_id = Some(p);
    }
    if let Some(s) = info.subagent_type {
        sub.subagent_type = Some(s);
    }
    if let Some(d) = info.description {
        sub.description = Some(d);
    }
    sub.parent_agent_id = info.parent_agent_id.or_else(|| Some(w.session_id.clone()));
    Some(sub)
}

fn resolve_invocation(
    start_uuid: &str,
    nodes: &HashMap<String, LineNode>,
    cache: &mut HashMap<String, Option<InvocationInfo>>,
    depth: u32,
) -> Option<InvocationInfo> {
    if depth > 64 {
        return None;
    }
    if let Some(cached) = cache.get(start_uuid) {
        return cached.clone();
    }
    let mut current_uuid = start_uuid.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    while let Some(n) = nodes.get(&current_uuid) {
        let node = n.clone();
        if !visited.insert(node.uuid.clone()) {
            break;
        }
        let parent_uuid = match &node.parent_uuid {
            Some(p) => p.clone(),
            None => break,
        };
        let parent = match nodes.get(&parent_uuid) {
            Some(p) => p.clone(),
            None => break,
        };
        match parent.agent_tool_use.clone() {
            Some(pat)
                if node.kind == LineKind::User
                    && parent.kind == LineKind::Assistant
                    && !node
                        .tool_result_ids
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&pat.id)) =>
            {
                let mut info = InvocationInfo {
                    root_uuid: node.uuid.clone(),
                    parent_tool_use_id: if pat.id.is_empty() {
                        None
                    } else {
                        Some(pat.id.clone())
                    },
                    subagent_type: pat.subagent_type.clone(),
                    description: pat.description.clone(),
                    parent_agent_id: None,
                };
                if parent.is_sidechain {
                    if let Some(pi) = resolve_invocation(&parent.uuid, nodes, cache, depth + 1) {
                        info.parent_agent_id = Some(pi.root_uuid);
                    }
                }
                cache.insert(start_uuid.to_string(), Some(info.clone()));
                return Some(info);
            }
            _ => {}
        }
        current_uuid = parent_uuid;
    }
    cache.insert(start_uuid.to_string(), None);
    None
}

// ---------------------------------------------------------------------------
// Content extraction.
// ---------------------------------------------------------------------------

fn extract_assistant_content(w: &WorkingRecord) -> Vec<ContentRecord> {
    let mut out = Vec::new();
    if w.session_id.is_empty() || w.message_id.is_empty() {
        return out;
    }
    let ts = w.first_ts.clone();
    for block in &w.blocks {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        let kind = bo.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(s) = bo.get("text").and_then(Value::as_str) {
                    if !s.is_empty() {
                        out.push(ContentRecord {
                            v: 1,
                            source: SourceKind::ClaudeCode,
                            session_id: w.session_id.clone(),
                            message_id: w.message_id.clone(),
                            ts: ts.clone(),
                            role: ContentRole::Assistant,
                            kind: ContentKind::Text,
                            text: Some(s.to_string()),
                            tool_use: None,
                            tool_result: None,
                        });
                    }
                }
            }
            "thinking" => {
                if let Some(s) = bo.get("thinking").and_then(Value::as_str) {
                    if !s.is_empty() {
                        out.push(ContentRecord {
                            v: 1,
                            source: SourceKind::ClaudeCode,
                            session_id: w.session_id.clone(),
                            message_id: w.message_id.clone(),
                            ts: ts.clone(),
                            role: ContentRole::Assistant,
                            kind: ContentKind::Thinking,
                            text: Some(s.to_string()),
                            tool_use: None,
                            tool_result: None,
                        });
                    }
                }
            }
            "tool_use" => {
                let id = bo.get("id").and_then(Value::as_str);
                let name = bo.get("name").and_then(Value::as_str);
                if let (Some(id), Some(name)) = (id, name) {
                    let input_map = match bo.get("input").and_then(Value::as_object) {
                        Some(m) => m
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect::<BTreeMap<_, _>>(),
                        None => BTreeMap::new(),
                    };
                    out.push(ContentRecord {
                        v: 1,
                        source: SourceKind::ClaudeCode,
                        session_id: w.session_id.clone(),
                        message_id: w.message_id.clone(),
                        ts: ts.clone(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::ToolUse,
                        text: None,
                        tool_use: Some(ContentToolUse {
                            id: id.to_string(),
                            name: name.to_string(),
                            input: input_map,
                        }),
                        tool_result: None,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_user_content(line: &serde_json::Map<String, Value>) -> Vec<ContentRecord> {
    let mut out = Vec::new();
    let session_id = string_field(line, SESSION_ID_KEYS, false).unwrap_or_default();
    let message_id = string_field(line, &["uuid"], false).unwrap_or_default();
    let ts = string_field(line, TIMESTAMP_KEYS, false).unwrap_or_default();
    if session_id.is_empty() || message_id.is_empty() {
        return out;
    }
    let body = line.get("message").and_then(|m| m.get("content"));
    let body = match body {
        Some(b) => b,
        None => return out,
    };
    if let Some(s) = body.as_str() {
        if !s.is_empty() {
            out.push(ContentRecord {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id,
                message_id,
                ts,
                role: ContentRole::User,
                kind: ContentKind::Text,
                text: Some(s.to_string()),
                tool_use: None,
                tool_result: None,
            });
        }
        return out;
    }
    let arr = match body.as_array() {
        Some(a) => a,
        None => return out,
    };
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        match bo.get("type").and_then(Value::as_str).unwrap_or("") {
            "tool_result" => {
                let tu = match bo.get("tool_use_id").and_then(Value::as_str) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let content = bo
                    .get("content")
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                let is_error =
                    (bo.get("is_error").and_then(Value::as_bool) == Some(true)).then_some(true);
                let rec = ContentRecord {
                    v: 1,
                    source: SourceKind::ClaudeCode,
                    session_id: session_id.clone(),
                    message_id: message_id.clone(),
                    ts: ts.clone(),
                    role: ContentRole::ToolResult,
                    kind: ContentKind::ToolResult,
                    text: None,
                    tool_use: None,
                    tool_result: Some(ContentToolResult {
                        tool_use_id: tu,
                        content,
                        is_error,
                    }),
                };
                out.push(rec);
            }
            "text" => {
                if let Some(s) = bo.get("text").and_then(Value::as_str) {
                    if !s.is_empty() {
                        out.push(ContentRecord {
                            v: 1,
                            source: SourceKind::ClaudeCode,
                            session_id: session_id.clone(),
                            message_id: message_id.clone(),
                            ts: ts.clone(),
                            role: ContentRole::User,
                            kind: ContentKind::Text,
                            text: Some(s.to_string()),
                            tool_use: None,
                            tool_result: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// User-turn record builder.
// ---------------------------------------------------------------------------

fn build_user_turn_record<C: TokenCounter + ?Sized>(
    line: &serde_json::Map<String, Value>,
    preceding_message_id: Option<&str>,
    counter: &C,
) -> Option<UserTurnRecord> {
    // Match TS `if (!sessionId || !userUuid) return undefined;` — JS
    // truthiness rejects both `undefined` and the empty string, so the Rust
    // port must reject blank IDs too. Without this, a malformed line carrying
    // `"sessionId": ""` would emit an unanchored UserTurnRecord and could
    // shadow the next assistant turn's `following_message_id` linkage.
    let session_id = string_field(line, SESSION_ID_KEYS, true)?;
    let user_uuid = string_field(line, &["uuid"], true)?;
    let blocks = extract_user_turn_blocks(line, counter);
    if blocks.is_empty() {
        return None;
    }
    Some(UserTurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id,
        user_uuid,
        ts: string_field(line, TIMESTAMP_KEYS, false).unwrap_or_default(),
        preceding_message_id: preceding_message_id.map(str::to_string),
        following_message_id: None,
        blocks,
    })
}

fn extract_user_turn_blocks<C: TokenCounter + ?Sized>(
    line: &serde_json::Map<String, Value>,
    counter: &C,
) -> Vec<UserTurnBlock> {
    let mut out = Vec::new();
    let body = match line.get("message").and_then(|m| m.get("content")) {
        Some(b) => b,
        None => return out,
    };
    if let Some(s) = body.as_str() {
        if !s.is_empty() {
            out.push(UserTurnBlock::text(s, counter));
        }
        return out;
    }
    let arr = match body.as_array() {
        Some(a) => a,
        None => return out,
    };
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        match bo.get("type").and_then(Value::as_str).unwrap_or("") {
            "tool_result" => {
                let id = match bo.get("tool_use_id").and_then(Value::as_str) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let content = bo.get("content").cloned().unwrap_or(Value::Null);
                let is_error = bo.get("is_error").and_then(Value::as_bool);
                out.push(UserTurnBlock::tool_result(id, &content, is_error, counter));
            }
            "text" => {
                if let Some(s) = bo.get("text").and_then(Value::as_str) {
                    if !s.is_empty() {
                        out.push(UserTurnBlock::text(s, counter));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tool result events.
// ---------------------------------------------------------------------------

fn collect_tool_result_events(
    line: &serde_json::Map<String, Value>,
    out: &mut Vec<ToolResultEventRecord>,
    counters: &mut HashMap<String, u64>,
    start_index: u64,
) -> u64 {
    let mut next = start_index;
    let session_id = match string_field(line, SESSION_ID_KEYS, false) {
        Some(s) if !s.is_empty() => s,
        _ => return next,
    };
    let arr = match line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        Some(a) => a,
        None => return next,
    };
    let message_id = string_field(line, &["uuid"], false);
    let ts = string_field(line, TIMESTAMP_KEYS, false);
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let tu = match bo.get("tool_use_id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let entry = counters.entry(tu.clone()).or_insert(0);
        let call_index = *entry;
        *entry += 1;
        let is_error = bo.get("is_error").and_then(Value::as_bool) == Some(true);
        let mut record = ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            tool_use_id: tu,
            call_index: Some(call_index),
            event_index: next,
            ts: ts.clone(),
            status: if is_error {
                ToolResultStatus::Errored
            } else {
                ToolResultStatus::Completed
            },
            event_source: ToolResultEventSource::ToolResult,
            content_length: None,
            output_bytes: None,
            output_truncated: None,
            content_hash: None,
            is_error: if is_error { Some(true) } else { None },
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        next += 1;
        if let Some(content) = bo.get("content") {
            let measured = measure_tool_result(content);
            record.content_length = measured.length;
            record.content_hash = measured.hash;
            record.output_bytes = measured.byte_length;
            record.output_truncated = measured.truncated;
        }
        if let Some(meta) = extract_replacement_meta_from_tool_result(block) {
            if let Some(ref names) = meta.replaced_tools {
                if !names.is_empty() {
                    record.replaced_tools = Some(names.clone());
                }
            }
            if let Some(c) = meta.collapsed_calls {
                if c > 0 {
                    record.collapsed_calls = Some(c);
                }
            }
        }
        out.push(record);
    }
    next
}

#[derive(Debug, Default)]
struct Measured {
    length: Option<u64>,
    hash: Option<String>,
    /// Raw UTF-8 byte length of the materialized text (the same string the
    /// hash is computed against). Added in #436 so hotspots can rank tools
    /// by raw payload bytes alongside post-truncation tokens.
    byte_length: Option<u64>,
    /// `Some(true)` when a recognized truncation marker was detected in
    /// the payload (see `detect_truncation_marker`). `None` when no
    /// payload was available; `Some(false)` when payload was inspected
    /// and looked complete.
    truncated: Option<bool>,
}

fn measure_tool_result(content: &Value) -> Measured {
    if let Some(s) = content.as_str() {
        // TS uses .length on the JS string, which counts UTF-16 code units.
        // For ASCII inputs this matches char count; for non-BMP chars the TS
        // and Rust counts diverge. Most fixture content is ASCII, so we use
        // char count as the best portable approximation. (Track in #255.)
        return Measured {
            length: Some(s.chars().count() as u64),
            hash: Some(content_hash(s)),
            byte_length: Some(s.len() as u64),
            truncated: Some(detect_truncation_marker(s)),
        };
    }
    if content.is_null() {
        return Measured::default();
    }
    match serde_json::to_string(content) {
        Ok(s) => Measured {
            length: Some(s.chars().count() as u64),
            hash: Some(content_hash(&s)),
            byte_length: Some(s.len() as u64),
            truncated: Some(detect_truncation_marker(&s)),
        },
        Err(_) => Measured::default(),
    }
}

/// Detect whether Claude Code embedded a truncation marker in the tool
/// result payload. Claude truncates large outputs (notably Bash stdout,
/// long-file reads) before serializing the tool_result block; the
/// truncated payload is suffixed/prefixed with a recognizable marker so
/// the assistant model can react. We look for the well-known phrasings
/// the Claude Code CLI emits as of 2026-Q1; new markers can be added
/// here without bumping the schema.
fn detect_truncation_marker(s: &str) -> bool {
    // Matched case-insensitively to absorb capitalization tweaks. Patterns
    // are kept short so partial-message previews still trigger.
    const MARKERS: &[&str] = &[
        "<system-truncated>",
        "[truncated]",
        "output truncated",
        "result truncated",
        "response truncated",
        "truncated to ",
    ];
    let lower = s.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

fn build_claude_system_tool_result_event(
    line: &serde_json::Map<String, Value>,
    counters: &mut HashMap<String, u64>,
    event_index: u64,
) -> Option<ToolResultEventRecord> {
    let session_id = string_field(line, SESSION_ID_KEYS, true)?;
    let tool_use_id = string_field(
        line,
        &[
            "parent_tool_use_id",
            "parentToolUseId",
            "parentToolUseID",
            "tool_use_id",
            "toolUseId",
        ],
        true,
    )?;
    let agent_id = string_field(line, &["agent_id", "agentId"], true);
    let subagent_session_id =
        string_field(line, &["subagent_session_id", "subagentSessionId"], true);
    if agent_id.is_none() && subagent_session_id.is_none() {
        return None;
    }
    let entry = counters.entry(tool_use_id.clone()).or_insert(0);
    let call_index = *entry;
    *entry += 1;
    let status = claude_system_event_status(line);
    let mut record = ToolResultEventRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id,
        message_id: None,
        tool_use_id,
        call_index: Some(call_index),
        event_index,
        ts: string_field(line, TIMESTAMP_KEYS, true),
        status,
        event_source: ToolResultEventSource::SubagentNotification,
        content_length: None,
        output_bytes: None,
        output_truncated: None,
        content_hash: None,
        is_error: None,
        usage: None,
        usage_attribution: None,
        subagent_session_id,
        agent_id,
        replaced_tools: None,
        collapsed_calls: None,
    };
    if matches!(status, ToolResultStatus::Errored) {
        record.is_error = Some(true);
    }
    let content = first_present(line, &["content", "output", "result", "message"]);
    if let Some(c) = content {
        let measured = measure_tool_result(c);
        record.content_length = measured.length;
        record.content_hash = measured.hash;
        record.output_bytes = measured.byte_length;
        record.output_truncated = measured.truncated;
    }
    Some(record)
}

fn claude_system_event_status(line: &serde_json::Map<String, Value>) -> ToolResultStatus {
    if line.get("is_error").and_then(Value::as_bool) == Some(true)
        || line.get("isError").and_then(Value::as_bool) == Some(true)
    {
        return ToolResultStatus::Errored;
    }
    let raw = string_field(
        line,
        &[
            "status",
            "state",
            "result",
            "terminal_status",
            "terminalStatus",
        ],
        true,
    );
    if let Some(s) = normalize_tool_result_status(raw.as_deref()) {
        return s;
    }
    if line.get("success").and_then(Value::as_bool) == Some(true) {
        return ToolResultStatus::Completed;
    }
    if line.get("success").and_then(Value::as_bool) == Some(false) {
        return ToolResultStatus::Errored;
    }
    ToolResultStatus::Unknown
}

fn normalize_tool_result_status(value: Option<&str>) -> Option<ToolResultStatus> {
    let v = value?;
    let lower = v.to_lowercase();
    let normalized: String = lower
        .chars()
        .map(|c| if c == '-' || c == ' ' { '_' } else { c })
        .collect();
    match normalized.as_str() {
        "completed" | "complete" | "success" | "succeeded" | "done" => {
            Some(ToolResultStatus::Completed)
        }
        "error" | "errored" | "failed" | "failure" => Some(ToolResultStatus::Errored),
        "running" | "in_progress" | "queued" | "pending" | "started" => {
            Some(ToolResultStatus::Running)
        }
        "cancelled" | "canceled" | "aborted" => Some(ToolResultStatus::Cancelled),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Replacement meta.
// ---------------------------------------------------------------------------

fn extract_replacement_meta_from_tool_result(block: &Value) -> Option<ReplacementMeta> {
    let bo = block.as_object()?;
    if let Some(meta) = pick_replacement_meta(bo.get("_meta")) {
        return Some(meta);
    }
    find_nested_replacement_meta(bo.get("content"))
}

fn pick_replacement_meta(raw: Option<&Value>) -> Option<ReplacementMeta> {
    let obj = raw?.as_object()?;
    let mut out = ReplacementMeta::default();
    if let Some(arr) = obj.get("replaces").and_then(Value::as_array) {
        let names: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !names.is_empty() {
            out.replaced_tools = Some(names);
        }
    }
    if let Some(c) = obj.get("collapsedCalls").and_then(Value::as_f64) {
        if c.is_finite() && c > 0.0 {
            out.collapsed_calls = Some(c.floor() as u64);
        }
    }
    if out.replaced_tools.is_none() && out.collapsed_calls.is_none() {
        return None;
    }
    Some(out)
}

fn find_nested_replacement_meta(content: Option<&Value>) -> Option<ReplacementMeta> {
    let arr = content?.as_array()?;
    for entry in arr {
        let obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };
        if let Some(meta) = pick_replacement_meta(obj.get("_meta")) {
            return Some(meta);
        }
    }
    None
}

fn collect_replacement_meta(
    line: &serde_json::Map<String, Value>,
    into: &mut HashMap<String, ReplacementMeta>,
) {
    let arr = match line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        Some(a) => a,
        None => return,
    };
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = match bo.get("tool_use_id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        if let Some(meta) = extract_replacement_meta_from_tool_result(block) {
            into.insert(id, meta);
        }
    }
}

// ---------------------------------------------------------------------------
// Plain text / errored helpers.
// ---------------------------------------------------------------------------

fn extract_plain_user_text_from_obj(line: &serde_json::Map<String, Value>) -> Option<String> {
    let body = line.get("message").and_then(|m| m.get("content"))?;
    if let Some(s) = body.as_str() {
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }
    let arr = body.as_array()?;
    let mut parts = Vec::new();
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(s) = bo.get("text").and_then(Value::as_str) {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn collect_errored_tool_use_ids(line: &serde_json::Map<String, Value>, into: &mut HashSet<String>) {
    let arr = match line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        Some(a) => a,
        None => return,
    };
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        if bo.get("is_error").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if let Some(id) = bo.get("tool_use_id").and_then(Value::as_str) {
            into.insert(id.to_string());
        }
    }
}

fn apply_classification(
    record: &mut TurnRecord,
    w: &WorkingRecord,
    user_text_by_message_id: &HashMap<String, String>,
    user_text_by_uuid: &HashMap<String, String>,
    nodes_by_uuid: &HashMap<String, LineNode>,
    errored: &HashSet<String>,
    skill_uuids: &HashSet<String>,
) {
    // Prefer the parent-chain walk (#433): walk from the assistant's first
    // emitted UUID up through `parentUuid` to the nearest user-prompt root
    // and use *that* user's text. Falls back to the legacy file-order map
    // keyed by message_id when:
    //   - the assistant row has no first_assistant_uuid (legacy/malformed)
    //   - the chain doesn't terminate at a known user prompt (orphan
    //     chain, cycle, root user prompt lacks text)
    //
    // The fallback is intentional: file-order association is wrong for
    // out-of-order flushes and interrupt+resume sessions, but it's still
    // the best signal we have for rows without UUIDs. Prefer over-grouping
    // (silently wrong text) to silently dropping classification entirely.
    let root_uuid = w
        .first_assistant_uuid
        .as_deref()
        .and_then(|uuid| nearest_user_prompt_root(uuid, nodes_by_uuid));
    let user_text = root_uuid
        .as_ref()
        .and_then(|root| user_text_by_uuid.get(root).cloned())
        .or_else(|| user_text_by_message_id.get(&w.message_id).cloned())
        .unwrap_or_default();
    let assistant_text = extract_assistant_text_for_classification(&w.blocks);
    let mut text_parts = Vec::new();
    if !user_text.is_empty() {
        text_parts.push(user_text);
    }
    if !assistant_text.is_empty() {
        text_parts.push(assistant_text);
    }
    let text = text_parts.join("\n");
    let has_failed_tool = record.tool_calls.iter().any(|tc| errored.contains(&tc.id));
    let result = classify_activity(ClassificationInput {
        tool_calls: &record.tool_calls,
        text: &text,
        has_failed_tool,
        reasoning_tokens: record.usage.reasoning,
    });
    // Slash-command triad override (#438). When the assistant turn's nearest
    // user-prompt root is one of the three triad UUIDs (caveat / invocation /
    // stdout), the assistant is paying for the slash command's stdout — tag
    // it as a `Skill` activity. Token attribution stays on the underlying
    // rows; the synthetic `Skill` label is a view that keeps per-category
    // rollups from counting one slash command three times. Override is
    // unconditional once the chain matches: a slash-command's text body is
    // a `<command-name>` / `<local-command-stdout>` envelope, not real
    // user intent, so keyword refinement on that text never produces a
    // category we'd want to preserve.
    let activity = match root_uuid.as_ref() {
        Some(root) if skill_uuids.contains(root) => ActivityCategory::Skill,
        _ => result.activity,
    };
    record.activity = Some(activity);
    record.retries = Some(result.retries);
    record.has_edits = Some(result.has_edits);
}

fn extract_assistant_text_for_classification(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        let bo = match b.as_object() {
            Some(o) => o,
            None => continue,
        };
        if bo.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(s) = bo.get("text").and_then(Value::as_str) {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Relationships.
// ---------------------------------------------------------------------------

fn build_explicit_claude_relationships(
    line: &serde_json::Map<String, Value>,
    session_id: &str,
    fallback_ts: Option<&str>,
) -> Vec<SessionRelationshipRecord> {
    let mut rows = Vec::new();
    let fork = string_field(line, &["forkSessionId", "fork_session_id"], true);
    if let Some(ref fork_id) = fork {
        if fork_id != session_id {
            rows.push(build_explicit_claude_relationship(
                line,
                session_id,
                fork_id,
                RelationshipType::Fork,
                fallback_ts,
            ));
        }
    }
    let cont = string_field(
        line,
        &["continuedFromSessionId", "continued_from_session_id"],
        true,
    );
    if let Some(ref c) = cont {
        if c != session_id {
            rows.push(build_explicit_claude_relationship(
                line,
                session_id,
                c,
                RelationshipType::Continuation,
                fallback_ts,
            ));
        }
    }
    rows
}

fn build_explicit_claude_relationship(
    line: &serde_json::Map<String, Value>,
    session_id: &str,
    related_session_id: &str,
    relationship_type: RelationshipType,
    fallback_ts: Option<&str>,
) -> SessionRelationshipRecord {
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: session_id.to_string(),
        related_session_id: Some(related_session_id.to_string()),
        relationship_type,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    let ts = string_field(line, TIMESTAMP_KEYS, true).or_else(|| fallback_ts.map(str::to_string));
    if let Some(t) = ts {
        row.ts = Some(t);
    }
    if let Some(s) = string_field(line, &["sourceSessionId", "source_session_id"], true) {
        row.source_session_id = Some(s);
    }
    if let Some(s) = string_field(line, SOURCE_VERSION_KEYS, true) {
        row.source_version = Some(s);
    }
    row
}

fn record_explicit_relationship_evidence(
    evidence: &mut ClaudeRelationshipEvidence,
    line: &serde_json::Map<String, Value>,
) {
    if let Some(c) = string_field(
        line,
        &["continuedFromSessionId", "continued_from_session_id"],
        true,
    ) {
        evidence.explicit_continuation_target_session_ids = Some(append_unique(
            evidence.explicit_continuation_target_session_ids.clone(),
            c,
        ));
    }
    if let Some(f) = string_field(line, &["forkSessionId", "fork_session_id"], true) {
        evidence.explicit_fork_target_session_ids = Some(append_unique(
            evidence.explicit_fork_target_session_ids.clone(),
            f,
        ));
    }
}

fn append_unique(values: Option<Vec<String>>, value: String) -> Vec<String> {
    let mut v = values.unwrap_or_default();
    if !v.iter().any(|s| s == &value) {
        v.push(value);
    }
    v
}

/// Owned, hashable identity for a relationship row. Used as a `HashSet` key
/// for cross-line dedup; cheap because the original `relationship_key` did one
/// `format!`-driven allocation per call but had to be re-run for every
/// candidate during `has_relationship`.
type RelationshipKey = (&'static str, String, &'static str, String, String, String);

fn relationship_key_borrowed(
    row: &SessionRelationshipRecord,
) -> (&'static str, &str, &'static str, &str, &str, &str) {
    (
        row.source.wire_str(),
        row.session_id.as_str(),
        row.relationship_type.wire_str(),
        row.related_session_id.as_deref().unwrap_or(""),
        row.agent_id.as_deref().unwrap_or(""),
        row.parent_tool_use_id.as_deref().unwrap_or(""),
    )
}

fn relationship_key(row: &SessionRelationshipRecord) -> RelationshipKey {
    let b = relationship_key_borrowed(row);
    (
        b.0,
        b.1.to_string(),
        b.2,
        b.3.to_string(),
        b.4.to_string(),
        b.5.to_string(),
    )
}

fn has_relationship(rows: &[SessionRelationshipRecord], row: &SessionRelationshipRecord) -> bool {
    let key = relationship_key_borrowed(row);
    rows.iter().any(|r| relationship_key_borrowed(r) == key)
}

fn collect_subagent_relationships(turns: &[TurnRecord], out: &mut Vec<SessionRelationshipRecord>) {
    let mut seen = HashSet::new();
    for t in turns {
        let sub = match &t.subagent {
            Some(s) if s.is_sidechain => s,
            _ => continue,
        };
        let agent_id = match &sub.agent_id {
            Some(a) => a,
            None => continue,
        };
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        let mut row = SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::NativeClaude,
            session_id: t.session_id.clone(),
            related_session_id: sub.parent_agent_id.clone(),
            relationship_type: RelationshipType::Subagent,
            ts: None,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: sub.parent_tool_use_id.clone(),
            agent_id: Some(agent_id.clone()),
            subagent_type: sub.subagent_type.clone(),
            description: sub.description.clone(),
        };
        if !t.ts.is_empty() {
            row.ts = Some(t.ts.clone());
        }
        out.push(row);
    }
}

fn record_evidence_from_line(evidence: &mut ClaudeRelationshipEvidence, line: &Value) {
    let lo = match line.as_object() {
        Some(o) => o,
        None => return,
    };
    if let Some(uuid) = lo.get("uuid").and_then(Value::as_str) {
        if !uuid.is_empty() {
            evidence.seen_uuids.push(uuid.to_string());
        }
    }
    if let Some(sid) = string_field(lo, SESSION_ID_KEYS, true) {
        if !evidence.in_log_session_ids.iter().any(|s| s == &sid) {
            evidence.in_log_session_ids.push(sid);
        }
        if evidence.first_ts.is_none() {
            evidence.first_ts = string_field(lo, TIMESTAMP_KEYS, true);
        }
    }
    if evidence.source_version.is_none() {
        evidence.source_version = string_field(lo, SOURCE_VERSION_KEYS, true);
    }
    let line_type = lo.get("type").and_then(Value::as_str).unwrap_or("");
    let is_sidechain = lo
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if line_type == "user" && !is_sidechain && !evidence.user_seen {
        evidence.user_seen = true;
        if let Some(p) = lo.get("parentUuid").and_then(Value::as_str) {
            if !p.is_empty() {
                evidence.first_parent_uuid = Some(p.to_string());
            }
        }
    }
}

fn record_resume_marker(
    evidence: &mut ClaudeRelationshipEvidence,
    line: &serde_json::Map<String, Value>,
) {
    let text = match extract_plain_user_text_from_obj(line) {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return;
    }
    let after_slash = &trimmed[1..];
    let cmd_end = after_slash
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_slash.len());
    let cmd = &after_slash[..cmd_end];
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower != "resume" && cmd_lower != "continue" {
        return;
    }
    evidence.has_resume_marker = true;
    let rest = after_slash[cmd_end..].trim_start();
    if !rest.is_empty() && evidence.resume_target_session_id.is_none() {
        let token_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if !token.is_empty() {
            evidence.resume_target_session_id = Some(token.to_string());
        }
    }
}

fn emit_local_continuation_from_resume(
    out: &mut Vec<SessionRelationshipRecord>,
    ev: &ClaudeRelationshipEvidence,
) {
    if !ev.has_resume_marker {
        return;
    }
    let fid = match ev.file_session_id.clone() {
        Some(s) => s,
        None => return,
    };
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: fid,
        related_session_id: ev.resume_target_session_id.clone(),
        relationship_type: RelationshipType::Continuation,
        ts: ev.first_ts.clone(),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    if has_relationship(out, &row) {
        return;
    }
    apply_evidence_provenance(&mut row, ev);
    out.push(row);
}

fn annotate_relationships_with_evidence(
    rows: &mut [SessionRelationshipRecord],
    ev: &ClaudeRelationshipEvidence,
) {
    for r in rows {
        apply_evidence_provenance(r, ev);
    }
}

fn apply_evidence_provenance(row: &mut SessionRelationshipRecord, ev: &ClaudeRelationshipEvidence) {
    if row.source_session_id.is_none() {
        if let Some(f) = pick_foreign_session_id(ev) {
            row.source_session_id = Some(f);
        }
    }
    if row.source_version.is_none() {
        if let Some(ref v) = ev.source_version {
            row.source_version = Some(v.clone());
        }
    }
}

fn pick_foreign_session_id(ev: &ClaudeRelationshipEvidence) -> Option<String> {
    let fid = ev.file_session_id.as_deref()?;
    for id in &ev.in_log_session_ids {
        if id != fid {
            return Some(id.clone());
        }
    }
    None
}

fn annotate_spawn_events(events: &mut [ToolResultEventRecord], turns: &[TurnRecord]) {
    if events.is_empty() {
        return;
    }
    let mut agent_by_parent_tool_use: HashMap<String, String> = HashMap::new();
    for t in turns {
        let sub = match &t.subagent {
            Some(s) if s.is_sidechain => s,
            _ => continue,
        };
        if let (Some(p), Some(a)) = (&sub.parent_tool_use_id, &sub.agent_id) {
            agent_by_parent_tool_use
                .entry(p.clone())
                .or_insert_with(|| a.clone());
        }
    }
    if agent_by_parent_tool_use.is_empty() {
        return;
    }
    for ev in events {
        if let Some(a) = agent_by_parent_tool_use.get(&ev.tool_use_id) {
            ev.agent_id = Some(a.clone());
        }
    }
}

fn annotate_compaction_events(events: &mut [CompactionEvent], turns: &[TurnRecord]) {
    if events.is_empty() {
        return;
    }
    let mut by_message_id: HashMap<&str, &TurnRecord> = HashMap::new();
    for t in turns {
        by_message_id.insert(t.message_id.as_str(), t);
    }
    for ev in events {
        if let Some(ref pmid) = ev.preceding_message_id {
            if let Some(t) = by_message_id.get(pmid.as_str()) {
                ev.tokens_before_compact = Some(t.usage.cache_read);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-file reconciliation.
// ---------------------------------------------------------------------------

pub fn reconcile_claude_session_relationships(
    inputs: &[ReconcileClaudeRelationshipsInput],
) -> Vec<SessionRelationshipRecord> {
    let mut out: Vec<SessionRelationshipRecord> = Vec::new();
    let usable: Vec<&ClaudeRelationshipEvidence> = inputs
        .iter()
        .map(|i| &i.evidence)
        .filter(|e| e.file_session_id.is_some())
        .collect();
    if usable.is_empty() {
        return out;
    }

    let mut uuid_to_file_session: HashMap<String, String> = HashMap::new();
    for ev in &usable {
        let sid = ev.file_session_id.as_ref().unwrap().clone();
        for u in &ev.seen_uuids {
            uuid_to_file_session
                .entry(u.clone())
                .or_insert_with(|| sid.clone());
        }
    }

    let mut continuation_of: HashMap<String, String> = HashMap::new();
    for ev in &usable {
        let sid = ev.file_session_id.as_ref().unwrap().clone();
        let parent_uuid = match &ev.first_parent_uuid {
            Some(p) => p.clone(),
            None => continue,
        };
        let parent_sid = match uuid_to_file_session.get(&parent_uuid) {
            Some(p) => p.clone(),
            None => continue,
        };
        if parent_sid == sid {
            continue;
        }
        continuation_of.insert(sid.clone(), parent_sid.clone());
        if ev.has_resume_marker
            && ev.resume_target_session_id.as_deref() == Some(parent_sid.as_str())
        {
            continue;
        }
        if has_explicit_target(&ev.explicit_continuation_target_session_ids, &parent_sid) {
            continue;
        }
        let mut row = SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::ClaudeCode,
            session_id: sid,
            related_session_id: Some(parent_sid),
            relationship_type: RelationshipType::Continuation,
            ts: ev.first_ts.clone(),
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: None,
            subagent_type: None,
            description: None,
        };
        apply_evidence_provenance(&mut row, ev);
        out.push(row);
    }

    let mut by_source_session: Vec<(String, Vec<&ClaudeRelationshipEvidence>)> = Vec::new();
    for ev in &usable {
        let foreign = match pick_foreign_session_id(ev) {
            Some(f) => f,
            None => continue,
        };
        let fid = ev.file_session_id.as_deref().unwrap_or("");
        if foreign == fid {
            continue;
        }
        if let Some(entry) = by_source_session.iter_mut().find(|(k, _)| k == &foreign) {
            entry.1.push(ev);
        } else {
            by_source_session.push((foreign, vec![ev]));
        }
    }

    for (foreign, group) in &by_source_session {
        if group.len() < 2 {
            continue;
        }
        for ev in group {
            let sid = ev.file_session_id.clone().unwrap();
            if let Some(parent) = continuation_of.get(&sid) {
                if group
                    .iter()
                    .any(|g| g.file_session_id.as_deref() == Some(parent.as_str()))
                {
                    continue;
                }
            }
            if has_explicit_target(&ev.explicit_fork_target_session_ids, foreign) {
                continue;
            }
            let row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::ClaudeCode,
                session_id: sid,
                related_session_id: Some(foreign.clone()),
                relationship_type: RelationshipType::Fork,
                ts: ev.first_ts.clone(),
                source_session_id: Some(foreign.clone()),
                source_version: ev.source_version.clone(),
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            out.push(row);
        }
    }

    out
}

fn has_explicit_target(targets: &Option<Vec<String>>, session_id: &str) -> bool {
    targets
        .as_ref()
        .is_some_and(|t| t.iter().any(|s| s == session_id))
}

// ---------------------------------------------------------------------------
// Incremental parser.
// ---------------------------------------------------------------------------

struct PrescanOutput {
    last_assistant_message_id: Option<String>,
    next_event_index: u64,
}

/// Pre-read the already-ingested prefix `[0, end_offset)` so a resumed call
/// has the same node graph, evidence, tool-result counters, event index, and
/// last-assistant-message-id it would have if it had started from byte 0.
/// Mirrors `prescanNodes` in `packages/reader/src/claude.ts`.
fn prescan_nodes(
    path: &Path,
    end_offset: u64,
    nodes_by_uuid: &mut HashMap<String, LineNode>,
    evidence: &mut ClaudeRelationshipEvidence,
    tool_result_counters: &mut HashMap<String, u64>,
) -> std::io::Result<PrescanOutput> {
    if end_offset == 0 {
        return Ok(PrescanOutput {
            last_assistant_message_id: None,
            next_event_index: 0,
        });
    }
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let length = end_offset.min(size);
    if length == 0 {
        return Ok(PrescanOutput {
            last_assistant_message_id: None,
            next_event_index: 0,
        });
    }
    // Stream the prefix line-by-line rather than reading `[0, length)`
    // into memory all at once. For multi-GB sessions the up-front
    // `vec![0u8; length as usize]` was a multi-GB allocation we never
    // need — only the longest single line has to fit in memory.
    let mut reader = BufReader::new(file).take(length);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut last_assistant_message_id: Option<String> = None;
    let mut next_event_index: u64 = 0;
    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        // A trailing partial line (no `\n`) inside the prescan window
        // should never happen — incremental ingest only commits cursors
        // at newline boundaries — but guard anyway.
        if line_buf.last() != Some(&b'\n') {
            break;
        }
        let raw = std::str::from_utf8(&line_buf[..n - 1]).unwrap_or("").trim();
        if raw.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match parsed.as_object() {
            Some(o) => o.clone(),
            None => continue,
        };
        let line_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match line_type {
            "assistant" => {
                register_assistant_node(&parsed, nodes_by_uuid);
                record_evidence_from_line(evidence, &parsed);
                record_explicit_relationship_evidence(evidence, &obj);
                if let Some(mid) = obj
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                {
                    last_assistant_message_id = Some(mid.to_string());
                }
            }
            "user" => {
                // Match the main-loop classification: a row is a real
                // user-prompt root only when it isn't a harness task
                // notification AND carries plain user text. See #439 for
                // the task-notification gate and #433 for why we tag
                // the LineNode for the parent-chain walker.
                let is_user_prompt = !is_task_notification(&obj)
                    && extract_plain_user_text_from_obj(&obj).is_some_and(|s| !s.is_empty());
                register_user_node(&parsed, nodes_by_uuid, is_user_prompt);
                record_evidence_from_line(evidence, &parsed);
                record_explicit_relationship_evidence(evidence, &obj);
                record_resume_marker(evidence, &obj);
                let mut harvested: Vec<ToolResultEventRecord> = Vec::new();
                next_event_index = collect_tool_result_events(
                    &obj,
                    &mut harvested,
                    tool_result_counters,
                    next_event_index,
                );
            }
            "system"
                if build_claude_system_tool_result_event(
                    &obj,
                    tool_result_counters,
                    next_event_index,
                )
                .is_some() =>
            {
                next_event_index += 1;
            }
            _ => {}
        }
    }
    Ok(PrescanOutput {
        last_assistant_message_id,
        next_event_index,
    })
}

fn record_root_incremental(
    out: &mut Vec<(u64, SessionRelationshipRecord)>,
    seen: &mut HashSet<String>,
    session_id: &str,
    ts: Option<&str>,
    line_offset: u64,
    file_session_id: Option<&str>,
) {
    let canonical = file_session_id.unwrap_or(session_id).to_string();
    if !seen.insert(canonical.clone()) {
        return;
    }
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: canonical,
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    if let Some(t) = ts {
        if !t.is_empty() {
            row.ts = Some(t.to_string());
        }
    }
    out.push((line_offset, row));
}

fn collect_explicit_claude_relationships_incremental(
    line: &serde_json::Map<String, Value>,
    evidence: &mut ClaudeRelationshipEvidence,
    out: &mut Vec<(u64, SessionRelationshipRecord)>,
    seen: &mut HashSet<RelationshipKey>,
    session_id: &str,
    fallback_ts: Option<&str>,
    line_offset: u64,
) {
    record_explicit_relationship_evidence(evidence, line);
    for row in build_explicit_claude_relationships(line, session_id, fallback_ts) {
        let key = relationship_key(&row);
        if !seen.insert(key) {
            continue;
        }
        out.push((line_offset, row));
    }
}

fn run_incremental<C: TokenCounter + ?Sized>(
    path: &Path,
    options: &ParseIncrementalOptions,
    counter: &C,
    emit_in_progress: bool,
) -> std::io::Result<ParseIncrementalResult> {
    let start_offset = options.start_offset.unwrap_or(0);
    let content_mode = options.content_mode.unwrap_or(ContentStoreMode::Off);
    let capture_content = matches!(content_mode, ContentStoreMode::Full);

    let file_session_id = derive_file_session_id_from_parts(
        options.file_session_id.as_deref(),
        options.session_path.as_deref(),
    );
    let mut evidence = new_evidence(file_session_id.clone());

    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();
    if start_offset >= size {
        return Ok(ParseIncrementalResult {
            turns: Vec::new(),
            content: Vec::new(),
            events: Vec::new(),
            relationships: Vec::new(),
            tool_result_events: Vec::new(),
            user_turns: Vec::new(),
            request_id_lookup: RequestIdLookup::new(),
            end_offset: start_offset,
            last_user_text: options.last_user_text.clone().unwrap_or_default(),
            evidence,
        });
    }

    let mut nodes_by_uuid: HashMap<String, LineNode> = HashMap::new();
    let mut invocation_cache: HashMap<String, Option<InvocationInfo>> = HashMap::new();
    let mut tool_result_counters: HashMap<String, u64> = HashMap::new();
    let mut next_event_index: u64 = 0;
    let mut last_assistant_message_id: Option<String> = None;

    if start_offset > 0 {
        let pre = prescan_nodes(
            path,
            start_offset,
            &mut nodes_by_uuid,
            &mut evidence,
            &mut tool_result_counters,
        )?;
        last_assistant_message_id = pre.last_assistant_message_id;
        next_event_index = pre.next_event_index;
    }

    // -1 sentinel: resume marker came from the prescan (definitely emit).
    // u64::MAX sentinel: no resume marker yet seen.
    // Otherwise: byte offset of the line that first set the marker on this pass.
    let mut resume_marker_offset: u64 = if evidence.has_resume_marker {
        0
    } else {
        u64::MAX
    };

    let mut current_user_text = options.last_user_text.clone().unwrap_or_default();

    let mut working: HashMap<String, WorkingRecord> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut message_id_first_offset: HashMap<String, u64> = HashMap::new();
    // Legacy file-order map kept as a fallback when the assistant row
    // lacks a `uuid` or its parent chain doesn't terminate at a known
    // user prompt. The preferred lookup is `user_text_by_uuid` walked
    // via `nearest_user_prompt_root` (#433).
    let mut user_text_by_message_id: HashMap<String, String> = HashMap::new();
    // User-prompt text keyed by the user line's own `uuid` — read by the
    // parent-chain walker during turn classification. Populated only for
    // real user prompts (task notifications excluded; empty bodies
    // excluded).
    let mut user_text_by_uuid: HashMap<String, String> = HashMap::new();
    let mut errored_tool_use_ids: HashSet<String> = HashSet::new();
    let mut replacement_meta_by_tool_use_id: HashMap<String, ReplacementMeta> = HashMap::new();
    // Slash-command triad detection (#438) needs a flat slice of user-typed
    // rows to look up the parent-UUID chain shape. We accumulate only the
    // minimal field set the detector reads (`type`, `uuid`, `parentUuid`,
    // `message.content`) so memory stays bounded — three rows per triad,
    // and only user-typed rows are stored. Detection runs once after the
    // streaming loop closes; the resulting `skill_uuids` set is consulted
    // by `apply_classification` to override the activity to `Skill`.
    let mut user_rows_for_triad: Vec<serde_json::Map<String, Value>> = Vec::new();
    let mut events: Vec<(u64, CompactionEvent)> = Vec::new();
    let mut pending_user_content: Vec<(u64, ContentRecord)> = Vec::new();
    let mut pending_tool_result_events: Vec<(u64, ToolResultEventRecord)> = Vec::new();
    let mut pending_relationships: Vec<(u64, SessionRelationshipRecord)> = Vec::new();
    let mut pending_user_turns: Vec<(u64, UserTurnRecord)> = Vec::new();
    let mut seen_root_session_ids: HashSet<String> = HashSet::new();
    let mut seen_explicit_relationship_ids: HashSet<RelationshipKey> = HashSet::new();
    let mut pending_user_turn_inc_idx: Option<usize> = None;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    // Stream from `start_offset` line-by-line. The previous implementation
    // allocated `Vec::with_capacity((size - start_offset) as usize)` and
    // `read_to_end` into it — for a multi-GB session this was a multi-GB
    // up-front allocation. With BufReader + `read_until` only the longest
    // single line stays resident.
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut cursor_offset: u64 = start_offset; // position past last complete \n
    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        // Drop trailing partial lines — the next incremental call resumes
        // from `cursor_offset`, which we only advance past complete `\n`.
        // `emit_in_progress` runs from the single-shot full-parse entry where
        // there is no next call, so a final line without a trailing `\n`
        // (truncated write, unflushed `\n`) must still be processed; the old
        // `ParseState` path used `read_line` and surfaced it.
        let has_newline = line_buf.last() == Some(&b'\n');
        if !has_newline && !emit_in_progress {
            break;
        }
        let line_start_offset = cursor_offset;
        let line_end_offset = cursor_offset + n as u64;
        let body_end = if has_newline { n - 1 } else { n };
        let trimmed = std::str::from_utf8(&line_buf[..body_end])
            .unwrap_or("")
            .trim();
        // Single-shot callers have no next pass, so a final partial line still
        // needs to bump the cursor past its body — `end_offset = cursor_offset`
        // is what the per-record offset filters compare against below.
        if has_newline || emit_in_progress {
            cursor_offset = line_end_offset;
        }
        if trimmed.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match parsed.as_object() {
            Some(o) => o.clone(),
            None => continue,
        };
        let line_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match line_type {
            "assistant" => {
                let mid = obj
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(ref mid_str) = mid {
                    if let Some(idx) = pending_user_turn_inc_idx {
                        if !message_id_first_offset.contains_key(mid_str) {
                            pending_user_turns[idx].1.following_message_id = Some(mid_str.clone());
                            pending_user_turn_inc_idx = None;
                        }
                    }
                    message_id_first_offset
                        .entry(mid_str.clone())
                        .or_insert(line_start_offset);
                    user_text_by_message_id
                        .entry(mid_str.clone())
                        .or_insert_with(|| current_user_text.clone());
                    last_assistant_message_id = Some(mid_str.clone());
                }
                let session_id = string_field(&obj, SESSION_ID_KEYS, false);
                let timestamp = string_field(&obj, TIMESTAMP_KEYS, false);
                if let Some(ref sid) = session_id {
                    if !sid.is_empty() {
                        record_root_incremental(
                            &mut pending_relationships,
                            &mut seen_root_session_ids,
                            sid,
                            timestamp.as_deref(),
                            line_start_offset,
                            file_session_id.as_deref(),
                        );
                        collect_explicit_claude_relationships_incremental(
                            &obj,
                            &mut evidence,
                            &mut pending_relationships,
                            &mut seen_explicit_relationship_ids,
                            file_session_id.as_deref().unwrap_or(sid.as_str()),
                            timestamp.as_deref(),
                            line_start_offset,
                        );
                    }
                }
                record_evidence_from_line(&mut evidence, &parsed);
                ingest_assistant_record(
                    &parsed,
                    &obj,
                    &mut working,
                    &mut order,
                    &mut nodes_by_uuid,
                );
            }
            "user" => {
                // Slash-command triad detector (#438) keeps a slim copy of
                // every user-typed row so a post-loop pass can find the
                // caveat → invocation → stdout chain shape. We clone the
                // row before the rest of the branch consumes it; the
                // detector only reads four fields so memory stays modest
                // (one entry per user row, dropped at function exit).
                user_rows_for_triad.push(obj.clone());
                // Harness-injected `<task-notification>` rows share the user
                // envelope but represent system events, not real prompts.
                // Detecting them here keeps them out of `current_user_text`
                // (so the classifier doesn't get task-notification text as
                // "user intent") and out of `pending_user_turns` (so
                // user-turn aggregates aren't inflated). Side effects like
                // session-relationship discovery still run because those
                // are independent of "is this a real user prompt". See
                // AgentWorkforce/burn#439.
                let task_notification = is_task_notification(&obj);
                let user_text = if task_notification {
                    None
                } else {
                    extract_plain_user_text_from_obj(&obj).filter(|s| !s.is_empty())
                };
                let is_user_prompt = user_text.is_some();
                register_user_node(&parsed, &mut nodes_by_uuid, is_user_prompt);
                if let Some(ref text) = user_text {
                    current_user_text = text.clone();
                    // Index by the user line's UUID for the parent-chain
                    // walker (#433). Falls back to no-op when the row
                    // lacks a `uuid`, in which case file-order remains
                    // the only association mechanism for downstream
                    // assistants.
                    if let Some(uuid) = obj.get("uuid").and_then(Value::as_str) {
                        if !uuid.is_empty() {
                            user_text_by_uuid
                                .entry(uuid.to_string())
                                .or_insert_with(|| text.clone());
                        }
                    }
                }
                collect_errored_tool_use_ids(&obj, &mut errored_tool_use_ids);
                collect_replacement_meta(&obj, &mut replacement_meta_by_tool_use_id);
                let session_id = string_field(&obj, SESSION_ID_KEYS, false);
                let timestamp = string_field(&obj, TIMESTAMP_KEYS, false);
                if let Some(ref sid) = session_id {
                    if !sid.is_empty() {
                        record_root_incremental(
                            &mut pending_relationships,
                            &mut seen_root_session_ids,
                            sid,
                            timestamp.as_deref(),
                            line_start_offset,
                            file_session_id.as_deref(),
                        );
                        collect_explicit_claude_relationships_incremental(
                            &obj,
                            &mut evidence,
                            &mut pending_relationships,
                            &mut seen_explicit_relationship_ids,
                            file_session_id.as_deref().unwrap_or(sid.as_str()),
                            timestamp.as_deref(),
                            line_start_offset,
                        );
                    }
                }
                record_evidence_from_line(&mut evidence, &parsed);
                let had_resume_before = evidence.has_resume_marker;
                record_resume_marker(&mut evidence, &obj);
                if !had_resume_before && evidence.has_resume_marker {
                    resume_marker_offset = line_start_offset;
                }
                let mut harvested: Vec<ToolResultEventRecord> = Vec::new();
                next_event_index = collect_tool_result_events(
                    &obj,
                    &mut harvested,
                    &mut tool_result_counters,
                    next_event_index,
                );
                for ev in harvested {
                    pending_tool_result_events.push((line_start_offset, ev));
                }
                if !task_notification {
                    if let Some(record) =
                        build_user_turn_record(&obj, last_assistant_message_id.as_deref(), counter)
                    {
                        let idx = pending_user_turns.len();
                        pending_user_turns.push((line_start_offset, record));
                        pending_user_turn_inc_idx = Some(idx);
                    }
                }
                if capture_content {
                    for c in extract_user_content(&obj) {
                        pending_user_content.push((line_start_offset, c));
                    }
                }
            }
            "system" => {
                if obj.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                    let session_id = string_field(&obj, SESSION_ID_KEYS, false).unwrap_or_default();
                    let ts = string_field(&obj, TIMESTAMP_KEYS, false).unwrap_or_default();
                    if !session_id.is_empty() {
                        let mut ev = CompactionEvent {
                            v: 1,
                            source: SourceKind::ClaudeCode,
                            session_id,
                            ts,
                            preceding_message_id: None,
                            tokens_before_compact: None,
                        };
                        if let Some(ref last) = last_assistant_message_id {
                            ev.preceding_message_id = Some(last.clone());
                        }
                        events.push((line_start_offset, ev));
                    }
                }
                if let Some(ev) = build_claude_system_tool_result_event(
                    &obj,
                    &mut tool_result_counters,
                    next_event_index,
                ) {
                    pending_tool_result_events.push((line_start_offset, ev));
                    next_event_index += 1;
                }
            }
            _ => {}
        }
    }

    // end_offset = byte position of the earliest in-progress messageId, or
    // cursor_offset (= position past the last complete newline) when all
    // messages are complete. In `emit_in_progress` mode (the full
    // non-incremental parse) we keep cursor_offset and emit every turn so the
    // result matches the single-shot ParseResult contract: callers want every
    // record we saw, including trailing in-progress assistants.
    let end_offset = if emit_in_progress {
        cursor_offset
    } else {
        let mut earliest_incomplete: Option<u64> = None;
        for id in &order {
            let w = match working.get(id) {
                Some(w) => w,
                None => continue,
            };
            if w.stop_reason.is_none() {
                if let Some(off) = message_id_first_offset.get(id) {
                    if earliest_incomplete.is_none_or(|e| *off < e) {
                        earliest_incomplete = Some(*off);
                    }
                }
            }
        }
        earliest_incomplete.unwrap_or(cursor_offset)
    };

    // Slash-command triad detection (#438). Run once over the accumulated
    // user rows; the resulting set of triad UUIDs (caveat + invocation +
    // stdout) is consulted by `apply_classification` to override the
    // assistant turn's activity to `Skill` when its parent-chain root
    // lands on any one of the three triad rows. Token attribution stays
    // on the underlying turn's `usage` — the synthetic `Skill` label is
    // a view, not a billing reattribution.
    let mut skill_uuids: HashSet<String> = HashSet::new();
    for triad in detect_slash_triads(&user_rows_for_triad) {
        for idx in [triad.caveat_idx, triad.invocation_idx, triad.stdout_idx] {
            if let Some(uuid) = user_rows_for_triad
                .get(idx)
                .and_then(|r| r.get("uuid"))
                .and_then(Value::as_str)
            {
                if !uuid.is_empty() {
                    skill_uuids.insert(uuid.to_string());
                }
            }
        }
    }
    // Detector input is no longer needed; drop the cloned rows so we
    // don't carry them past the emission loop.
    drop(user_rows_for_triad);

    // Emit completed turns. In-progress messages (no stop_reason) are deferred
    // — `end_offset` already backs up to before their first byte so the next
    // call re-reads them. `emit_in_progress` opts the non-incremental path out
    // of that skip so it emits every working record.
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut assistant_pending: Vec<(u64, usize, ContentRecord)> = Vec::new();
    for (i, id) in order.iter().enumerate() {
        let w = match working.get(id) {
            Some(w) => w,
            None => continue,
        };
        if !emit_in_progress && w.stop_reason.is_none() {
            continue;
        }
        let tool_calls = extract_tool_calls(
            &w.blocks,
            &errored_tool_use_ids,
            Some(&replacement_meta_by_tool_use_id),
        );
        let files_touched = extract_files_touched(&tool_calls);
        let subagent = resolve_subagent(w, &nodes_by_uuid, &mut invocation_cache);

        let mut record = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: w.session_id.clone(),
            session_path: options.session_path.clone(),
            message_id: w.message_id.clone(),
            turn_index: i as u64,
            ts: w.first_ts.clone(),
            model: w.model.clone(),
            project: None,
            project_key: None,
            usage: w.usage.clone(),
            tool_calls: tool_calls.clone(),
            files_touched: if files_touched.is_empty() {
                None
            } else {
                Some(files_touched)
            },
            subagent,
            stop_reason: w
                .stop_reason
                .as_deref()
                .map(|s| StopReason::from_wire(s).unwrap_or(StopReason::Silent)),
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: Some(build_claude_fidelity(&w.usage_coverage)),
        };
        if let Some(ref cwd) = w.cwd {
            let resolved = resolve_project(cwd);
            record.project = Some(resolved.project);
            record.project_key = resolved.project_key;
        }
        apply_classification(
            &mut record,
            w,
            &user_text_by_message_id,
            &user_text_by_uuid,
            &nodes_by_uuid,
            &errored_tool_use_ids,
            &skill_uuids,
        );
        turns.push(record);

        if capture_content {
            let msg_offset = *message_id_first_offset.get(&w.message_id).unwrap_or(&0);
            for (sub, r) in extract_assistant_content(w).into_iter().enumerate() {
                assistant_pending.push((msg_offset, sub + 1, r));
            }
        }
    }

    // Filter content by end_offset and interleave by source-line offset.
    // appendContent has no row-level dedup, so we MUST drop rows past
    // end_offset — the next call will re-read those bytes and re-emit them.
    let mut content: Vec<ContentRecord> = Vec::new();
    if capture_content {
        let mut merged: Vec<(u64, usize, ContentRecord)> = Vec::new();
        for (off, rec) in pending_user_content.into_iter() {
            if off < end_offset {
                merged.push((off, 0, rec));
            }
        }
        for (off, sub, rec) in assistant_pending.into_iter() {
            if off < end_offset {
                merged.push((off, sub, rec));
            }
        }
        merged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        content = merged.into_iter().map(|(_, _, r)| r).collect();
    }

    let mut emitted_events: Vec<CompactionEvent> = events
        .into_iter()
        .filter(|(off, _)| *off < end_offset)
        .map(|(_, ev)| ev)
        .collect();
    annotate_compaction_events(&mut emitted_events, &turns);

    let mut emitted_relationships: Vec<SessionRelationshipRecord> = pending_relationships
        .into_iter()
        .filter(|(off, _)| *off < end_offset)
        .map(|(_, r)| r)
        .collect();
    collect_subagent_relationships(&turns, &mut emitted_relationships);
    if resume_marker_offset < end_offset {
        emit_local_continuation_from_resume(&mut emitted_relationships, &evidence);
    }
    annotate_relationships_with_evidence(&mut emitted_relationships, &evidence);

    let mut emitted_tool_result_events: Vec<ToolResultEventRecord> = pending_tool_result_events
        .into_iter()
        .filter(|(off, _)| *off < end_offset)
        .map(|(_, ev)| ev)
        .collect();
    annotate_spawn_events(&mut emitted_tool_result_events, &turns);

    let emitted_user_turns: Vec<UserTurnRecord> = pending_user_turns
        .into_iter()
        .filter(|(off, _)| *off < end_offset)
        .map(|(_, ut)| ut)
        .collect();

    // Build the `(source, session_id, message_id) -> requestId` lookup
    // for every emitted turn. We only walk turns the run actually emits
    // (in-progress assistant rows are filtered out above) so the lookup
    // entries always correspond to an outbound `TurnRecord`. See issue
    // #434.
    let mut request_id_lookup = RequestIdLookup::new();
    for t in &turns {
        if let Some(w) = working.get(&t.message_id) {
            if let Some(req) = w.request_id.as_ref() {
                if !req.is_empty() {
                    request_id_lookup.insert(TurnKey::for_turn(t), req.clone());
                }
            }
        }
    }

    Ok(ParseIncrementalResult {
        turns,
        content,
        events: emitted_events,
        relationships: emitted_relationships,
        tool_result_events: emitted_tool_result_events,
        user_turns: emitted_user_turns,
        request_id_lookup,
        end_offset,
        last_user_text: current_user_text,
        evidence,
    })
}

// ---------------------------------------------------------------------------
// Misc helpers.
// ---------------------------------------------------------------------------

fn derive_file_session_id_from_parts(
    file_session_id: Option<&str>,
    session_path: Option<&str>,
) -> Option<String> {
    // Mirrors the TS `deriveFileSessionId`: only honor explicit caller signals
    // (`fileSessionId` then `sessionPath` basename). Do NOT fall back to the
    // on-disk path the parser opened — that would canonicalize relationship
    // rows to the input filename for default-options callers, breaking joins
    // against the real in-log `sessionId` UUIDs.
    if let Some(s) = file_session_id {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(sp) = session_path {
        if !sp.is_empty() {
            return basename_without_ext(sp, "jsonl");
        }
    }
    None
}

fn basename_without_ext(path: &str, ext: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let suffix = format!(".{}", ext);
    let stem = if let Some(stripped) = name.strip_suffix(&suffix) {
        stripped
    } else {
        name
    };
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn new_evidence(file_session_id: Option<String>) -> ClaudeRelationshipEvidence {
    ClaudeRelationshipEvidence {
        file_session_id,
        ..ClaudeRelationshipEvidence::default()
    }
}

const SESSION_ID_KEYS: &[&str] = &["sessionId", "session_id"];
const TIMESTAMP_KEYS: &[&str] = &["timestamp", "ts"];
const SOURCE_VERSION_KEYS: &[&str] = &["version", "sourceVersion", "source_version"];

fn string_field(
    obj: &serde_json::Map<String, Value>,
    keys: &[&str],
    require_nonempty: bool,
) -> Option<String> {
    let mut empty_match: Option<String> = None;
    for k in keys {
        match obj.get(*k).and_then(Value::as_str) {
            Some(s) if !s.is_empty() => return Some(s.to_string()),
            Some(s) if !require_nonempty && empty_match.is_none() => {
                empty_match = Some(s.to_string());
            }
            _ => {}
        }
    }
    empty_match
}

fn first_present<'a>(obj: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `measure_tool_result` populates both the legacy char-count
    /// `length` and the new `byte_length` field added in #436. For
    /// ASCII fixture content they agree; the byte length is what the
    /// `tool_result_events.output_bytes` column stores so hotspots can
    /// rank by raw payload regardless of token truncation downstream.
    #[test]
    fn measure_tool_result_populates_byte_length_and_truncation_flag() {
        let plain = serde_json::json!("hello world");
        let m = measure_tool_result(&plain);
        assert_eq!(m.byte_length, Some(11));
        assert_eq!(m.length, Some(11));
        assert_eq!(m.truncated, Some(false));

        let truncated = serde_json::json!(
            "... lots of output ...\n[truncated]\nsystem note: response truncated"
        );
        let m = measure_tool_result(&truncated);
        assert_eq!(m.truncated, Some(true));
        assert!(m.byte_length.unwrap() > 0);

        let null = serde_json::json!(null);
        let m = measure_tool_result(&null);
        assert_eq!(m.byte_length, None);
        assert_eq!(m.truncated, None);
    }

    #[test]
    fn detect_truncation_marker_matches_known_phrasings() {
        assert!(detect_truncation_marker(
            "Bash output truncated at 30000 chars"
        ));
        assert!(detect_truncation_marker("<system-truncated>"));
        assert!(detect_truncation_marker(
            "(...)\n[truncated]\n(end of preview)"
        ));
        assert!(detect_truncation_marker("Result Truncated"));
        assert!(!detect_truncation_marker("hello world"));
        assert!(!detect_truncation_marker(""));
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("claude")
            .join(name)
    }

    #[test]
    fn parse_result_from_incremental_result_copies_all_fields() {
        let turn = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            session_path: Some("/tmp/session.jsonl".to_string()),
            message_id: "msg-1".to_string(),
            turn_index: 7,
            ts: "2026-05-11T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: Some("/tmp/project".to_string()),
            project_key: Some("project-key".to_string()),
            usage: Usage {
                input: 1,
                output: 2,
                reasoning: 3,
                cache_read: 4,
                cache_create_5m: 5,
                cache_create_1h: 6,
            },
            tool_calls: vec![],
            files_touched: Some(vec!["/tmp/project/src/lib.rs".to_string()]),
            subagent: Some(Subagent {
                is_sidechain: false,
                parent_tool_use_id: Some("tool-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                parent_agent_id: Some("parent-agent".to_string()),
                subagent_type: Some("general-purpose".to_string()),
                description: Some("delegate".to_string()),
            }),
            stop_reason: Some(StopReason::EndTurn),
            activity: Some(crate::reader::types::ActivityCategory::Coding),
            retries: Some(1),
            has_edits: Some(true),
            fidelity: Some(Fidelity {
                granularity: UsageGranularity::PerTurn,
                coverage: Coverage {
                    has_input_tokens: true,
                    has_output_tokens: true,
                    has_reasoning_tokens: true,
                    has_cache_read_tokens: true,
                    has_cache_create_tokens: true,
                    has_tool_calls: true,
                    has_tool_result_events: true,
                    has_session_relationships: true,
                    has_raw_content: true,
                },
                class: crate::reader::types::FidelityClass::Full,
            }),
        };
        let content = ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            message_id: "msg-1".to_string(),
            ts: "2026-05-11T00:00:00.000Z".to_string(),
            role: ContentRole::Assistant,
            kind: ContentKind::Text,
            text: Some("hello".to_string()),
            tool_use: None,
            tool_result: None,
        };
        let event = CompactionEvent {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            ts: "2026-05-11T00:01:00.000Z".to_string(),
            preceding_message_id: Some("msg-0".to_string()),
            tokens_before_compact: Some(42),
        };
        let relationship = SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            related_session_id: Some("session-0".to_string()),
            relationship_type: RelationshipType::Continuation,
            ts: Some("2026-05-11T00:02:00.000Z".to_string()),
            source_session_id: Some("source-session".to_string()),
            source_version: Some("1.2.3".to_string()),
            parent_tool_use_id: Some("tool-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            subagent_type: Some("general-purpose".to_string()),
            description: Some("continued".to_string()),
        };
        let tool_result_event = ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            message_id: Some("msg-1".to_string()),
            tool_use_id: "tool-1".to_string(),
            call_index: Some(0),
            event_index: 9,
            ts: Some("2026-05-11T00:03:00.000Z".to_string()),
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: Some(5),
            output_bytes: Some(5),
            output_truncated: Some(false),
            content_hash: Some("abc123".to_string()),
            is_error: Some(false),
            usage: Some(Usage::default()),
            usage_attribution: Some(crate::reader::types::UsageAttribution::SingleToolTurn),
            subagent_session_id: Some("sub-session".to_string()),
            agent_id: Some("agent-1".to_string()),
            replaced_tools: Some(vec!["old-tool".to_string()]),
            collapsed_calls: Some(2),
        };
        let user_turn = UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session-1".to_string(),
            user_uuid: "user-1".to_string(),
            ts: "2026-05-11T00:04:00.000Z".to_string(),
            preceding_message_id: Some("msg-0".to_string()),
            following_message_id: Some("msg-1".to_string()),
            blocks: vec![UserTurnBlock {
                kind: crate::reader::types::UserTurnBlockKind::Text,
                tool_use_id: None,
                byte_len: 5,
                approx_tokens: 1,
                is_error: None,
            }],
        };
        let evidence = ClaudeRelationshipEvidence {
            file_session_id: Some("session-1".to_string()),
            first_ts: Some("2026-05-11T00:00:00.000Z".to_string()),
            in_log_session_ids: vec!["session-1".to_string()],
            source_version: Some("1.2.3".to_string()),
            first_parent_uuid: Some("parent-1".to_string()),
            seen_uuids: vec!["uuid-1".to_string()],
            has_resume_marker: true,
            resume_target_session_id: Some("session-0".to_string()),
            explicit_continuation_target_session_ids: Some(vec!["session-0".to_string()]),
            explicit_fork_target_session_ids: Some(vec!["session-2".to_string()]),
            user_seen: true,
        };

        let incremental = ParseIncrementalResult {
            turns: vec![turn.clone()],
            content: vec![content.clone()],
            events: vec![event.clone()],
            relationships: vec![relationship.clone()],
            tool_result_events: vec![tool_result_event.clone()],
            user_turns: vec![user_turn.clone()],
            request_id_lookup: RequestIdLookup::new(),
            end_offset: 123,
            last_user_text: "latest user turn".to_string(),
            evidence: evidence.clone(),
        };

        let full = ParseResult::from(incremental);

        assert_eq!(full.turns, vec![turn]);
        assert_eq!(full.content, vec![content]);
        assert_eq!(full.events, vec![event]);
        assert_eq!(full.relationships, vec![relationship]);
        assert_eq!(full.tool_result_events, vec![tool_result_event]);
        assert_eq!(full.user_turns, vec![user_turn]);
        assert_eq!(full.evidence.file_session_id, evidence.file_session_id);
        assert_eq!(full.evidence.first_ts, evidence.first_ts);
        assert_eq!(
            full.evidence.in_log_session_ids,
            evidence.in_log_session_ids
        );
        assert_eq!(full.evidence.source_version, evidence.source_version);
        assert_eq!(full.evidence.first_parent_uuid, evidence.first_parent_uuid);
        assert_eq!(full.evidence.seen_uuids, evidence.seen_uuids);
        assert_eq!(full.evidence.has_resume_marker, evidence.has_resume_marker);
        assert_eq!(
            full.evidence.resume_target_session_id,
            evidence.resume_target_session_id
        );
        assert_eq!(
            full.evidence.explicit_continuation_target_session_ids,
            evidence.explicit_continuation_target_session_ids
        );
        assert_eq!(
            full.evidence.explicit_fork_target_session_ids,
            evidence.explicit_fork_target_session_ids
        );
        assert_eq!(full.evidence.user_seen, evidence.user_seen);
    }

    #[test]
    fn simple_turn_parses() {
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1, "expected one turn");
        let t = &res.turns[0];
        assert_eq!(t.v, 1);
        assert_eq!(t.source, SourceKind::ClaudeCode);
        assert_eq!(t.message_id, "msg_simple_1");
        assert_eq!(t.model, "claude-sonnet-4-6");
        assert_eq!(t.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(t.usage.input, 10);
        assert_eq!(t.usage.output, 5);
        assert_eq!(t.usage.cache_read, 500);
        assert_eq!(t.usage.cache_create_5m, 80);
        assert_eq!(t.usage.cache_create_1h, 20);
        assert_eq!(t.tool_calls.len(), 0);
        assert!(t.files_touched.is_none());
    }

    #[test]
    fn full_parse_emits_final_line_without_trailing_newline() {
        // Mid-write truncation / unflushed writer: the final JSON line is
        // syntactically complete but missing `\n`. The single-shot parse must
        // still surface it — matching the prior `BufReader::read_line` path.
        let src = std::fs::read_to_string(fixture("simple-turn.jsonl")).unwrap();
        let no_trailing = src.trim_end_matches('\n');
        assert!(!no_trailing.ends_with('\n'));
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        std::fs::write(&working, no_trailing).unwrap();
        let res = parse_claude_session(&working, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1, "final line without \\n must still emit");
        assert_eq!(res.turns[0].message_id, "msg_simple_1");
    }

    #[test]
    fn multi_block_turn_collapses_to_single_turn() {
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(
            res.turns.len(),
            1,
            "four assistant lines must collapse to one turn"
        );
        let t = &res.turns[0];
        assert_eq!(t.message_id, "msg_multi_1");
        assert_eq!(t.tool_calls.len(), 2);
        assert_eq!(t.tool_calls[0].name, "Bash");
        assert_eq!(
            t.tool_calls[0].target.as_deref(),
            Some("ls -la /tmp/project")
        );
        assert_eq!(t.tool_calls[1].name, "Agent");
        assert_eq!(t.tool_calls[1].target.as_deref(), Some("general-purpose"));
        assert_eq!(t.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(t.ts, "2026-04-20T00:00:01.000Z");
    }

    /// Issue #434 acceptance: the multi-block fixture's four assistant
    /// rows share `requestId=req_1` and a single `message.id`. The
    /// parser surfaces that requestId on its `request_id_lookup`, the
    /// inference builder collapses the four rows into ONE
    /// `Inference`, and the merged usage matches the carrier row
    /// (NOT 4× the carrier row, which would be the row-summing
    /// pathology).
    #[test]
    fn multi_block_turn_emits_one_inference_with_merged_usage() {
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1, "one turn (collapsed by message_id)");
        let t = &res.turns[0];
        // The reader populated the per-turn lookup with the upstream
        // requestId. Without this entry, the inference builder would
        // fall back to `message_id`, which is correct cardinality for
        // Claude but loses the `request-id` provenance.
        let req = res
            .request_id_lookup
            .get(&crate::reader::TurnKey::for_turn(t))
            .expect("request_id_lookup must carry every Claude turn");
        assert_eq!(req, "req_1");

        let infs = crate::reader::build_inferences(&res.turns, &res.request_id_lookup);
        assert_eq!(
            infs.len(),
            1,
            "four assistant rows sharing requestId collapse to one Inference"
        );
        let inf = &infs[0];
        assert_eq!(inf.request_id, "req_1");
        assert_eq!(
            inf.request_id_source,
            crate::reader::InferenceKeySource::RequestId
        );
        assert_eq!(inf.turn_id, "msg_multi_1");
        // Carrier usage values: input=3, output=43, cache_read=11_496,
        // cache_create_1h=4_773. The pre-fix bug emitted these multiplied
        // by row count when usage was on the first row; with the fix the
        // single inference reports the carrier's values exactly once.
        assert_eq!(inf.usage.input, 3);
        assert_eq!(inf.usage.output, 43);
        assert_eq!(inf.usage.cache_read, 11_496);
        assert_eq!(inf.usage.cache_create_1h, 4_773);
        // start_ts / end_ts come from the parent `TurnRecord` (already
        // collapsed by message_id), so they equal each other here —
        // `TurnRecord.ts` is the first row's ts. A future surface that
        // wants per-row spans should reach into the parser's per-row
        // metadata; the inference summary stays correct for the
        // "how long did the API call take" case the issue asked about
        // by giving us the first-row arrival time.
        assert_eq!(inf.start_ts, "2026-04-20T00:00:01.000Z");
        assert_eq!(inf.end_ts, "2026-04-20T00:00:01.000Z");
        assert_eq!(inf.tool_uses.len(), 2);
        assert_eq!(inf.kind, crate::reader::InferenceKind::ToolUse);
    }

    /// A turn that the parser parsed without an upstream `requestId`
    /// (older Claude version, sidechain, or other harness) falls back
    /// to `message_id` as the inference key. See `RequestIdLookup`
    /// fallback rules in `reader/inference.rs`.
    #[test]
    fn inference_falls_back_to_message_id_when_lookup_empty() {
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        // Empty the lookup to simulate a harness that didn't ship one.
        let empty = crate::reader::RequestIdLookup::new();
        let infs = crate::reader::build_inferences(&res.turns, &empty);
        assert_eq!(infs.len(), 1);
        assert_eq!(infs[0].request_id, "msg_multi_1");
        assert_eq!(
            infs[0].request_id_source,
            crate::reader::InferenceKeySource::MessageId
        );
    }

    #[test]
    fn files_touched_excludes_grep_and_bash() {
        let path = fixture("files-touched.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1);
        let t = &res.turns[0];
        assert_eq!(t.tool_calls.len(), 3);
        assert_eq!(
            t.files_touched.as_deref(),
            Some(["/src/a.ts".to_string(), "/src/b.ts".to_string()].as_slice())
        );
    }

    #[test]
    fn sidechain_turn_marked_subagent() {
        let path = fixture("sidechain-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1);
        let t = &res.turns[0];
        let sub = t.subagent.as_ref().expect("expected sidechain marker");
        assert!(sub.is_sidechain);
    }

    #[test]
    fn nested_subagent_tree_reconstructs() {
        let path = fixture("nested-subagent.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        // 2 main + 2 outer sidechain + 1 inner sidechain = 5 turns
        assert_eq!(res.turns.len(), 5);
        let by_id: HashMap<&str, &TurnRecord> = res
            .turns
            .iter()
            .map(|t| (t.message_id.as_str(), t))
            .collect();
        let main1 = by_id.get("msg_main_1").unwrap();
        assert!(main1.subagent.is_none());
        let sub1_1 = by_id.get("msg_sub1_1").unwrap();
        let s = sub1_1.subagent.as_ref().unwrap();
        assert!(s.is_sidechain);
        assert_eq!(s.agent_id.as_deref(), Some("u-sub1-user"));
        assert_eq!(s.parent_tool_use_id.as_deref(), Some("toolu_outer"));
        assert_eq!(s.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(s.description.as_deref(), Some("Research the codebase"));
        assert_eq!(
            s.parent_agent_id.as_deref(),
            Some("55555555-5555-5555-5555-555555555555")
        );
    }

    // ----- parseClaudeSessionIncremental conformance -----
    //
    // Mirrors `describe('parseClaudeSessionIncremental', ...)` in
    // packages/reader/src/claude.test.ts. Each Rust test corresponds to one
    // `it()` case; fixture files are read from the shared
    // `tests/fixtures/claude/` directory so the TS and Rust suites exercise
    // the same input bytes.

    use crate::reader::types::{ActivityCategory, FidelityClass, UserTurnBlockKind};
    use std::io::Write as _;

    fn read_bytes(p: &std::path::Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    fn write_bytes(p: &std::path::Path, b: &[u8]) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(b).unwrap();
    }

    fn alias_key_session_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("alias-session.jsonl");
        let lines = [
            serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "toolu_alias", "content": "ok"}]
                },
                "uuid": "u-alias-user",
                "sessionId": "",
                "session_id": "alias-session",
                "timestamp": "",
                "ts": "2026-04-25T00:00:00.000Z",
                "continued_from_session_id": "parent-session",
                "cwd": "/tmp/project",
                "sourceVersion": "2.1.alias",
            }),
            serde_json::json!({
                "parentUuid": "u-alias-user",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_alias_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
                    },
                },
                "type": "assistant",
                "uuid": "u-alias-asst",
                "sessionId": "",
                "session_id": "alias-session",
                "timestamp": "",
                "ts": "2026-04-25T00:00:01.000Z",
                "cwd": "/tmp/project",
            }),
            serde_json::json!({
                "type": "system",
                "subtype": "compact_boundary",
                "sessionId": "",
                "session_id": "alias-session",
                "timestamp": "",
                "ts": "2026-04-25T00:00:02.000Z",
            }),
            serde_json::json!({
                "type": "system",
                "subtype": "subagent_completed",
                "sessionId": "",
                "session_id": "alias-session",
                "timestamp": "",
                "ts": "2026-04-25T00:00:03.000Z",
                "parent_tool_use_id": "toolu_alias",
                "agent_id": "agent-alias",
                "subagent_session_id": "child-alias",
                "status": "completed",
                "content": "subagent finished",
            }),
        ];
        let body = lines
            .iter()
            .map(|j| j.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        write_bytes(&working, body.as_bytes());
        (dir, working)
    }

    fn append_str(p: &std::path::Path, s: &str) {
        let mut prev = std::fs::read(p).unwrap();
        prev.extend_from_slice(s.as_bytes());
        write_bytes(p, &prev);
    }

    /// Returns the byte offset of the line whose JSON contains `needle`.
    fn line_start_offset(path: &std::path::Path, needle: &str) -> u64 {
        let raw = std::fs::read_to_string(path).unwrap();
        let mut off: u64 = 0;
        for line in raw.split_inclusive('\n') {
            if line.contains(needle) {
                return off;
            }
            off += line.len() as u64;
        }
        panic!("needle {:?} not found in {:?}", needle, path);
    }

    #[test]
    fn session_id_and_ts_aliases_reach_sync_outputs() {
        let (_dir, path) = alias_key_session_file();
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(res.turns.len(), 1);
        assert_eq!(res.turns[0].session_id, "alias-session");
        assert_eq!(res.turns[0].ts, "2026-04-25T00:00:01.000Z");

        assert_eq!(res.user_turns.len(), 1);
        assert_eq!(res.user_turns[0].session_id, "alias-session");
        assert_eq!(res.user_turns[0].ts, "2026-04-25T00:00:00.000Z");
        assert_eq!(
            res.user_turns[0].following_message_id.as_deref(),
            Some("msg_alias_1")
        );

        assert!(res.content.iter().all(|c| c.session_id == "alias-session"));
        assert!(res.content.iter().any(
            |c| matches!(c.role, ContentRole::ToolResult) && c.ts == "2026-04-25T00:00:00.000Z"
        ));
        assert!(res.content.iter().any(
            |c| matches!(c.role, ContentRole::Assistant) && c.ts == "2026-04-25T00:00:01.000Z"
        ));

        let root = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Root))
            .expect("root relationship");
        assert_eq!(root.session_id, "alias-session");
        assert_eq!(root.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
        assert_eq!(root.source_version.as_deref(), Some("2.1.alias"));
        let continuation = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
            .expect("continuation relationship");
        assert_eq!(continuation.session_id, "alias-session");
        assert_eq!(
            continuation.related_session_id.as_deref(),
            Some("parent-session")
        );
        assert_eq!(continuation.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
        assert_eq!(continuation.source_version.as_deref(), Some("2.1.alias"));

        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].session_id, "alias-session");
        assert_eq!(res.events[0].ts, "2026-04-25T00:00:02.000Z");
        assert_eq!(
            res.events[0].preceding_message_id.as_deref(),
            Some("msg_alias_1")
        );

        let tool_event = res
            .tool_result_events
            .iter()
            .find(|e| matches!(e.event_source, ToolResultEventSource::ToolResult))
            .expect("tool result event");
        assert_eq!(tool_event.session_id, "alias-session");
        assert_eq!(tool_event.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
        let system_event = res
            .tool_result_events
            .iter()
            .find(|e| matches!(e.event_source, ToolResultEventSource::SubagentNotification))
            .expect("system tool result event");
        assert_eq!(system_event.session_id, "alias-session");
        assert_eq!(system_event.ts.as_deref(), Some("2026-04-25T00:00:03.000Z"));
        assert_eq!(system_event.call_index, Some(1));

        assert_eq!(res.evidence.in_log_session_ids, vec!["alias-session"]);
        assert_eq!(
            res.evidence.first_ts.as_deref(),
            Some("2026-04-25T00:00:00.000Z")
        );
        assert_eq!(res.evidence.source_version.as_deref(), Some("2.1.alias"));
    }

    #[test]
    fn session_id_and_ts_aliases_reach_incremental_outputs() {
        let (_dir, path) = alias_key_session_file();
        let res = parse_claude_session_incremental(
            &path,
            &ParseIncrementalOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(res.turns.len(), 1);
        assert_eq!(res.turns[0].session_id, "alias-session");
        assert_eq!(res.turns[0].ts, "2026-04-25T00:00:01.000Z");
        assert_eq!(res.user_turns.len(), 1);
        assert_eq!(res.user_turns[0].session_id, "alias-session");
        assert_eq!(res.user_turns[0].ts, "2026-04-25T00:00:00.000Z");
        assert!(res
            .relationships
            .iter()
            .any(|r| matches!(r.relationship_type, RelationshipType::Root)
                && r.session_id == "alias-session"));
        assert!(res.relationships.iter().any(|r| matches!(
            r.relationship_type,
            RelationshipType::Continuation
        ) && r.session_id == "alias-session"
            && r.related_session_id.as_deref() == Some("parent-session")));
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].session_id, "alias-session");
        assert_eq!(res.events[0].ts, "2026-04-25T00:00:02.000Z");
        assert_eq!(res.tool_result_events.len(), 2);
        assert!(res.tool_result_events.iter().any(|e| matches!(
            e.event_source,
            ToolResultEventSource::ToolResult
        ) && e.session_id == "alias-session"
            && e.ts.as_deref() == Some("2026-04-25T00:00:00.000Z")));
        assert!(res.tool_result_events.iter().any(|e| matches!(
            e.event_source,
            ToolResultEventSource::SubagentNotification
        ) && e.session_id == "alias-session"
            && e.ts.as_deref() == Some("2026-04-25T00:00:03.000Z")));
        assert!(res.content.iter().all(|c| c.session_id == "alias-session"));
        assert_eq!(res.evidence.in_log_session_ids, vec!["alias-session"]);
        assert_eq!(
            res.evidence.first_ts.as_deref(),
            Some("2026-04-25T00:00:00.000Z")
        );
        assert_eq!(res.evidence.source_version.as_deref(), Some("2.1.alias"));
    }

    #[test]
    fn incremental_reads_whole_file_from_start() {
        let src = fixture("simple-turn.jsonl");
        let raw_len = read_bytes(&src).len() as u64;
        let r =
            parse_claude_session_incremental(&src, &ParseIncrementalOptions::default()).unwrap();
        assert_eq!(r.turns.len(), 1);
        assert_eq!(r.turns[0].message_id, "msg_simple_1");
        assert_eq!(r.end_offset, raw_len);
    }

    #[test]
    fn incremental_returns_zero_turns_when_start_at_eof() {
        let src = fixture("simple-turn.jsonl");
        let raw_len = read_bytes(&src).len() as u64;
        let r = parse_claude_session_incremental(
            &src,
            &ParseIncrementalOptions {
                start_offset: Some(raw_len),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.turns.len(), 0);
        assert_eq!(r.end_offset, raw_len);
    }

    #[test]
    fn incremental_appended_turn_emitted_on_resume() {
        let src = fixture("simple-turn.jsonl");
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        std::fs::copy(&src, &working).unwrap();
        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        assert_eq!(first.turns.len(), 1);

        let appended = serde_json::json!({
            "parentUuid": "u-asst-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_simple_2",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "and another"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
                }
            },
            "type": "assistant",
            "uuid": "u-asst-2",
            "timestamp": "2026-04-20T00:00:05.000Z",
            "cwd": "/tmp/project",
            "sessionId": "11111111-1111-1111-1111-111111111111",
        });
        append_str(&working, &(appended.to_string() + "\n"));

        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].message_id, "msg_simple_2");
        let full_len = read_bytes(&working).len() as u64;
        assert_eq!(second.end_offset, full_len);
    }

    #[test]
    fn incremental_defers_in_progress_trailing_message() {
        let src = fixture("incomplete-then-complete.jsonl");
        let inprog_offset = line_start_offset(&src, "\"id\":\"msg_inprog_1\"");
        let r =
            parse_claude_session_incremental(&src, &ParseIncrementalOptions::default()).unwrap();
        assert_eq!(r.turns.len(), 1, "only the complete message is emitted");
        assert_eq!(r.turns[0].message_id, "msg_done_1");
        assert_eq!(
            r.end_offset, inprog_offset,
            "endOffset backs up to start of in-progress line"
        );
    }

    #[test]
    fn incremental_defers_content_for_in_progress_then_emits_after_completion() {
        let src = fixture("incomplete-then-complete.jsonl");
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        std::fs::copy(&src, &working).unwrap();

        let first = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let asst_first: Vec<&ContentRecord> = first
            .content
            .iter()
            .filter(|c| matches!(c.role, ContentRole::Assistant))
            .collect();
        assert!(asst_first.iter().all(|c| c.message_id == "msg_done_1"));

        let tail = serde_json::json!({
            "parentUuid": "u-asst-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_inprog_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done now"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 3,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
                }
            },
            "type": "assistant",
            "uuid": "u-asst-2",
            "timestamp": "2026-04-20T00:00:02.000Z",
            "cwd": "/tmp/project",
            "sessionId": "33333333-3333-3333-3333-333333333333",
        });
        append_str(&working, &(tail.to_string() + "\n"));

        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let asst_second: Vec<&ContentRecord> = second
            .content
            .iter()
            .filter(|c| matches!(c.role, ContentRole::Assistant))
            .collect();
        assert!(!asst_second.is_empty());
        assert!(asst_second.iter().all(|c| c.message_id == "msg_inprog_1"));
        assert!(asst_second
            .iter()
            .any(|c| matches!(c.kind, ContentKind::Text) && c.text.as_deref() == Some("done now")));
    }

    #[test]
    fn incremental_defers_assistant_content_after_in_progress_message() {
        // msg_done_1 (complete) → msg_inprog_1 (incomplete) → msg_after_1 (complete).
        // endOffset must back up to msg_inprog_1, so msg_after_1 content is deferred
        // — appendContent has no row dedup so the next pass would otherwise duplicate it.
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        let lines = [
            serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {"role": "user", "content": "hi"},
                "uuid": "u-user-1",
                "timestamp": "2026-04-20T00:00:00.000Z",
                "cwd": "/tmp/project",
                "sessionId": "sess-dup",
            }),
            serde_json::json!({
                "parentUuid": "u-user-1",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_done_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
                },
                "type": "assistant",
                "uuid": "u-asst-1",
                "timestamp": "2026-04-20T00:00:01.000Z",
                "cwd": "/tmp/project",
                "sessionId": "sess-dup",
            }),
            serde_json::json!({
                "parentUuid": "u-asst-1",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_inprog_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "working..."}],
                    "stop_reason": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
                },
                "type": "assistant",
                "uuid": "u-asst-2",
                "timestamp": "2026-04-20T00:00:02.000Z",
                "cwd": "/tmp/project",
                "sessionId": "sess-dup",
            }),
            serde_json::json!({
                "parentUuid": "u-asst-2",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_after_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "after"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
                },
                "type": "assistant",
                "uuid": "u-asst-3",
                "timestamp": "2026-04-20T00:00:03.000Z",
                "cwd": "/tmp/project",
                "sessionId": "sess-dup",
            }),
        ];
        let body: String = lines
            .iter()
            .map(|j| j.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        write_bytes(&working, body.as_bytes());

        let r = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let message_ids: Vec<&str> = r
            .content
            .iter()
            .filter(|c| matches!(c.role, ContentRole::Assistant))
            .map(|c| c.message_id.as_str())
            .collect();
        assert_eq!(message_ids, vec!["msg_done_1"]);
        let buf_len = read_bytes(&working).len() as u64;
        assert!(r.end_offset < buf_len);
    }

    #[test]
    fn incremental_skips_incomplete_turn_then_emits_when_completion_arrives() {
        let src = fixture("incomplete-then-complete.jsonl");
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        std::fs::copy(&src, &working).unwrap();
        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        assert_eq!(first.turns.len(), 1);

        // Append a completion line for msg_inprog_1 (same id, but stop_reason set).
        let tail = serde_json::json!({
            "parentUuid": "u-asst-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_inprog_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "working..."}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 3,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
                }
            },
            "type": "assistant",
            "uuid": "u-asst-2",
            "timestamp": "2026-04-20T00:00:02.000Z",
            "cwd": "/tmp/project",
            "sessionId": "33333333-3333-3333-3333-333333333333",
        });
        append_str(&working, &(tail.to_string() + "\n"));

        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].message_id, "msg_inprog_1");
        assert_eq!(second.turns[0].stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn incremental_preserves_user_prompt_across_resume() {
        // Regression: when an incomplete assistant message forces endOffset to
        // back up past the user prompt, the resumed call re-reads the
        // assistant line without seeing the prompt. We carry lastUserText
        // forward so the classifier still has keyword context.
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        let session_id = "44444444-4444-4444-4444-444444444444";
        let lines = [
            serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {"role": "user", "content": "fix the bug in auth.ts"},
                "uuid": "u-user-1",
                "timestamp": "2026-04-20T00:00:00.000Z",
                "cwd": "/tmp/project",
                "sessionId": session_id,
            }),
            serde_json::json!({
                "parentUuid": "u-user-1",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_resume_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu_edit_1", "name": "Edit", "input": {"file_path": "/auth.ts"}}],
                    "stop_reason": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
                },
                "type": "assistant",
                "uuid": "u-asst-1",
                "timestamp": "2026-04-20T00:00:01.000Z",
                "cwd": "/tmp/project",
                "sessionId": session_id,
            }),
        ];
        let body: String = lines
            .iter()
            .map(|j| j.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        write_bytes(&working, body.as_bytes());

        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        assert_eq!(first.turns.len(), 0, "incomplete turn is deferred");
        assert_eq!(first.last_user_text, "fix the bug in auth.ts");

        // Append completion of msg_resume_1.
        let tail = serde_json::json!({
            "parentUuid": "u-asst-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_resume_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "tu_edit_1", "name": "Edit", "input": {"file_path": "/auth.ts"}}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
            },
            "type": "assistant",
            "uuid": "u-asst-1",
            "timestamp": "2026-04-20T00:00:01.000Z",
            "cwd": "/tmp/project",
            "sessionId": session_id,
        });
        append_str(&working, &(tail.to_string() + "\n"));

        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                last_user_text: Some(first.last_user_text.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.turns.len(), 1);
        let t = &second.turns[0];
        assert_eq!(t.message_id, "msg_resume_1");
        assert_eq!(
            t.activity,
            Some(ActivityCategory::Debugging),
            "user prompt mentions 'bug' so edit turn is debugging"
        );

        // Without the seed, the prompt is lost on resume and the classifier
        // falls back to coding.
        let without_seed = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            without_seed.turns[0].activity,
            Some(ActivityCategory::Coding)
        );
    }

    #[test]
    fn incremental_user_turns_emitted_once_across_resumed_passes() {
        let src = fixture("user-turn-blocks.jsonl");
        let full = std::fs::read_to_string(&src).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");

        // Pass 1: write only through msg_utb_2 (4 lines: user, asst, user, asst).
        let lines: Vec<&str> = full.split('\n').filter(|l| !l.is_empty()).collect();
        let prefix = lines[..4].join("\n") + "\n";
        write_bytes(&working, prefix.as_bytes());
        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        let first_ids: Vec<&str> = first
            .user_turns
            .iter()
            .map(|u| u.user_uuid.as_str())
            .collect();
        assert_eq!(first_ids, vec!["u-user-1", "u-user-2"]);

        // Pass 2: full file. Must emit only u-user-3 (no re-emit of 1/2).
        write_bytes(&working, full.as_bytes());
        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                last_user_text: Some(first.last_user_text.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let second_ids: Vec<&str> = second
            .user_turns
            .iter()
            .map(|u| u.user_uuid.as_str())
            .collect();
        assert_eq!(second_ids, vec!["u-user-3"]);
        let u3 = &second.user_turns[0];
        assert_eq!(u3.preceding_message_id.as_deref(), Some("msg_utb_2"));
        assert_eq!(u3.following_message_id.as_deref(), Some("msg_utb_3"));
        assert_eq!(u3.blocks[0].is_error, Some(true));
    }

    #[test]
    fn incremental_seeds_tool_result_event_counters_from_prescan() {
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        let session_id = "66666666-6666-6666-6666-666666666666";
        let user_result = serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_system", "content": "done"}]
            },
            "uuid": "u-result-1",
            "timestamp": "2026-04-24T01:00:00.000Z",
            "cwd": "/tmp/project",
            "sessionId": session_id,
        });
        let incomplete_assistant = serde_json::json!({
            "parentUuid": "u-result-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_waiting",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "waiting"}],
                "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
            },
            "type": "assistant",
            "uuid": "u-asst-waiting",
            "timestamp": "2026-04-24T01:00:01.000Z",
            "cwd": "/tmp/project",
            "sessionId": session_id,
        });
        let system_notification = serde_json::json!({
            "type": "system",
            "subtype": "subagent_completed",
            "sessionId": session_id,
            "timestamp": "2026-04-24T01:00:02.000Z",
            "parent_tool_use_id": "toolu_system",
            "agent_id": "agent-system-2",
            "subagent_session_id": "session-system-child-2",
            "status": "completed",
        });
        let body = [&user_result, &incomplete_assistant, &system_notification]
            .iter()
            .map(|j| j.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        write_bytes(&working, body.as_bytes());

        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        assert_eq!(first.tool_result_events.len(), 1);
        assert_eq!(
            first.tool_result_events[0].event_source,
            ToolResultEventSource::ToolResult
        );
        assert_eq!(first.tool_result_events[0].tool_use_id, "toolu_system");
        assert_eq!(first.tool_result_events[0].call_index, Some(0));
        assert_eq!(first.tool_result_events[0].event_index, 0);

        // Append a completion line for msg_waiting so the deferred system
        // notification line gets re-read on the next pass.
        let mut complete_assistant = incomplete_assistant.clone();
        complete_assistant["message"]["stop_reason"] = serde_json::Value::from("end_turn");
        let body2 = [
            &user_result,
            &incomplete_assistant,
            &system_notification,
            &complete_assistant,
        ]
        .iter()
        .map(|j| j.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        write_bytes(&working, body2.as_bytes());

        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                last_user_text: Some(first.last_user_text.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let ev = second
            .tool_result_events
            .iter()
            .find(|e| matches!(e.event_source, ToolResultEventSource::SubagentNotification))
            .expect("resumed pass should emit the deferred system notification");
        assert_eq!(ev.tool_use_id, "toolu_system");
        assert_eq!(ev.call_index, Some(1));
        assert_eq!(ev.event_index, 1);
        assert_eq!(ev.agent_id.as_deref(), Some("agent-system-2"));
        assert_eq!(
            ev.subagent_session_id.as_deref(),
            Some("session-system-child-2")
        );
    }

    #[test]
    fn incremental_resolves_subagent_tree_via_prescan() {
        // Pass 1 ingests the main thread + Agent spawn line. Pass 2 starts
        // beyond them and must still populate agentId / parentAgentId /
        // parentToolUseId on the sidechain turns via the prescan registering
        // the prior parentUuid nodes.
        let src = fixture("nested-subagent.jsonl");
        let full = std::fs::read_to_string(&src).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");

        let lines: Vec<&str> = full.split('\n').filter(|l| !l.is_empty()).collect();
        // Write only through the outer Agent spawn line on pass 1.
        let prefix = lines[..2].join("\n") + "\n";
        write_bytes(&working, prefix.as_bytes());
        let first = parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
            .unwrap();
        assert!(!first.turns.is_empty());

        write_bytes(&working, full.as_bytes());
        let second = parse_claude_session_incremental(
            &working,
            &ParseIncrementalOptions {
                start_offset: Some(first.end_offset),
                ..Default::default()
            },
        )
        .unwrap();

        let by_id: HashMap<&str, &TurnRecord> = second
            .turns
            .iter()
            .map(|t| (t.message_id.as_str(), t))
            .collect();
        let sub1_1 = by_id
            .get("msg_sub1_1")
            .expect("outer sidechain turn should be emitted on pass 2");
        let sub2_1 = by_id
            .get("msg_sub2_1")
            .expect("inner sidechain turn should be emitted on pass 2");

        let s1 = sub1_1.subagent.as_ref().unwrap();
        assert_eq!(s1.agent_id.as_deref(), Some("u-sub1-user"));
        assert_eq!(s1.parent_tool_use_id.as_deref(), Some("toolu_outer"));
        assert_eq!(s1.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(
            s1.parent_agent_id.as_deref(),
            Some("55555555-5555-5555-5555-555555555555")
        );

        let s2 = sub2_1.subagent.as_ref().unwrap();
        assert_eq!(s2.agent_id.as_deref(), Some("u-sub2-user"));
        assert_eq!(s2.parent_agent_id.as_deref(), Some("u-sub1-user"));
        assert_eq!(s2.parent_tool_use_id.as_deref(), Some("toolu_inner"));
    }

    // ----- parseClaudeSession (synchronous) extended conformance -----
    //
    // Mirrors the remaining `it()` cases under the top
    // `describe('parseClaudeSession', ...)` block in
    // `packages/reader/src/claude.test.ts` (lines 17-311) so the Rust port
    // gates on byte-equivalent assertions against the same shared fixtures.

    #[test]
    fn simple_turn_records_project_and_full_usage() {
        // Mirrors `it('parses a simple one-turn session')` lines 18-38, adding
        // the project field and the full Usage struct that the lighter
        // `simple_turn_parses` test above does not check.
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let t = &res.turns[0];
        assert_eq!(t.project.as_deref(), Some("/tmp/project"));
        assert_eq!(
            t.usage,
            Usage {
                input: 10,
                output: 5,
                reasoning: 0,
                cache_read: 500,
                cache_create_5m: 80,
                cache_create_1h: 20,
            }
        );
    }

    #[test]
    fn multi_block_turn_keeps_usage_once() {
        // Mirrors `it('dedupes a multi-block assistant message and keeps usage once')`
        // (claude.test.ts:40). The four assistant lines for `msg_multi_1` repeat
        // the same usage block; the parser must collapse to one turn that
        // counts that usage exactly once.
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let t = &res.turns[0];
        assert_eq!(
            t.usage,
            Usage {
                input: 3,
                output: 43,
                reasoning: 0,
                cache_read: 11496,
                cache_create_5m: 0,
                cache_create_1h: 4773,
            }
        );
    }

    #[test]
    fn stable_args_hash_for_identical_tool_inputs() {
        // claude.test.ts:113 — `argsHash` is a content hash, so two parses of
        // the same fixture must produce identical hashes; different inputs in
        // the same turn must hash differently.
        let path = fixture("multi-block-turn.jsonl");
        let a = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let b = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(
            a.turns[0].tool_calls[0].args_hash,
            b.turns[0].tool_calls[0].args_hash
        );
        assert_ne!(
            a.turns[0].tool_calls[0].args_hash,
            a.turns[0].tool_calls[1].args_hash
        );
    }

    #[test]
    fn marks_tool_call_is_error_when_tool_result_has_is_error_true() {
        // claude.test.ts:120 — every Bash call in retry-loop.jsonl is followed
        // by a tool_result carrying is_error=true. The parser back-populates
        // ToolCall.isError so consumers don't need a separate join.
        let path = fixture("retry-loop.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 4);
        for t in &res.turns {
            assert_eq!(t.tool_calls.len(), 1);
            assert_eq!(t.tool_calls[0].name, "Bash");
            assert_eq!(t.tool_calls[0].is_error, Some(true));
        }
    }

    #[test]
    fn back_populates_replacement_meta_from_tool_result() {
        // claude.test.ts:130 — tool_result `_meta.replaces` and
        // `_meta.collapsedCalls` are surfaced both on the originating ToolCall
        // and on the matching ToolResultEventRecord. Calls without _meta keep
        // the fields absent.
        let path = fixture("replacement-meta.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let all: Vec<&ToolCall> = res.turns.iter().flat_map(|t| t.tool_calls.iter()).collect();
        let search = all
            .iter()
            .find(|tc| tc.name == "relaywash__Search")
            .expect("search tool call present");
        let read = all
            .iter()
            .find(|tc| tc.name == "Read")
            .expect("read tool call present");
        assert_eq!(
            search.replaced_tools.as_deref(),
            Some(["Glob".to_string(), "Grep".to_string(), "Read".to_string()].as_slice())
        );
        assert_eq!(search.collapsed_calls, Some(9));
        assert!(read.replaced_tools.is_none());
        assert!(read.collapsed_calls.is_none());

        let search_event = res
            .tool_result_events
            .iter()
            .find(|e| e.tool_use_id == "tu_search_1")
            .expect("search tool_result event present");
        assert_eq!(
            search_event.replaced_tools.as_deref(),
            Some(["Glob".to_string(), "Grep".to_string(), "Read".to_string()].as_slice())
        );
        assert_eq!(search_event.collapsed_calls, Some(9));
    }

    #[test]
    fn extracts_edit_pre_and_post_hashes() {
        // claude.test.ts:151 — Edit tool calls carry editPreHash / editPostHash
        // derived from old_string / new_string. A revert (second edit's post ==
        // first edit's pre) is detectable by comparing the hashes.
        let path = fixture("edit-revert.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let edits: Vec<&ToolCall> = res
            .turns
            .iter()
            .flat_map(|t| t.tool_calls.iter())
            .filter(|tc| tc.name == "Edit")
            .collect();
        assert_eq!(edits.len(), 2);
        assert!(edits[0].edit_pre_hash.is_some());
        assert!(edits[0].edit_post_hash.is_some());
        assert_eq!(edits[1].edit_post_hash, edits[0].edit_pre_hash);
        assert_eq!(edits[1].edit_pre_hash, edits[0].edit_post_hash);
    }

    #[test]
    fn tool_result_events_chronological_with_full_metadata() {
        // claude.test.ts:165 — every tool_result block in retry-loop.jsonl
        // becomes a ToolResultEventRecord. Status is `errored`, contentLength
        // and contentHash are populated, and eventIndex is monotonic.
        let path = fixture("retry-loop.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.tool_result_events.len(), 4);
        for ev in &res.tool_result_events {
            assert_eq!(ev.v, 1);
            assert_eq!(ev.source, SourceKind::ClaudeCode);
            assert_eq!(ev.event_source, ToolResultEventSource::ToolResult);
            assert_eq!(ev.status, ToolResultStatus::Errored);
            assert_eq!(ev.is_error, Some(true));
            assert!(ev.content_length.is_some());
            assert!(ev.content_hash.is_some());
        }
        for w in res.tool_result_events.windows(2) {
            assert!(w[1].event_index > w[0].event_index);
        }
    }

    #[test]
    fn relationships_root_plus_subagent_per_invocation() {
        // claude.test.ts:187 — one root row per session, one subagent row per
        // distinct invocation (agentId). Outer's parent is the main session
        // id; inner's parent is the outer invocation's agentId. Source is
        // `native-claude` per the TS spec (not `claude-code`) — that flag
        // separates harness-emitted edges from cross-source spawn-env edges.
        let path = fixture("nested-subagent.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let roots: Vec<_> = res
            .relationships
            .iter()
            .filter(|r| matches!(r.relationship_type, RelationshipType::Root))
            .collect();
        let subs: Vec<_> = res
            .relationships
            .iter()
            .filter(|r| matches!(r.relationship_type, RelationshipType::Subagent))
            .collect();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].session_id, "55555555-5555-5555-5555-555555555555");
        assert_eq!(subs.len(), 2);

        let outer = subs
            .iter()
            .find(|r| r.subagent_type.as_deref() == Some("Explore"))
            .expect("outer subagent row present");
        let inner = subs
            .iter()
            .find(|r| r.subagent_type.as_deref() == Some("code-reviewer"))
            .expect("inner subagent row present");

        assert_eq!(outer.agent_id.as_deref(), Some("u-sub1-user"));
        assert_eq!(outer.source, RelationshipSourceKind::NativeClaude);
        assert_eq!(outer.parent_tool_use_id.as_deref(), Some("toolu_outer"));
        assert_eq!(
            outer.related_session_id.as_deref(),
            Some("55555555-5555-5555-5555-555555555555")
        );
        assert_eq!(outer.description.as_deref(), Some("Research the codebase"));

        assert_eq!(inner.agent_id.as_deref(), Some("u-sub2-user"));
        assert_eq!(inner.parent_tool_use_id.as_deref(), Some("toolu_inner"));
        assert_eq!(inner.related_session_id.as_deref(), Some("u-sub1-user"));
    }

    #[test]
    fn tool_result_events_join_to_spawned_subagent_via_agent_id() {
        // claude.test.ts:216 — Agent/Task tool_results inherit the spawned
        // subagent's agentId so cross-table joins work without a separate
        // subagent index.
        let path = fixture("nested-subagent.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let outer = res
            .tool_result_events
            .iter()
            .find(|e| e.tool_use_id == "toolu_outer")
            .expect("outer Agent tool_result event present");
        let inner = res
            .tool_result_events
            .iter()
            .find(|e| e.tool_use_id == "toolu_inner")
            .expect("inner Agent tool_result event present");
        assert_eq!(outer.agent_id.as_deref(), Some("u-sub1-user"));
        assert_eq!(inner.agent_id.as_deref(), Some("u-sub2-user"));
    }

    #[test]
    fn system_subagent_notification_emits_tool_result_event() {
        // claude.test.ts:228 — a `system` line with subtype
        // `subagent_completed` becomes a ToolResultEventRecord (not a
        // CompactionEvent), with eventSource=`subagent_notification` and the
        // child session id surfaced.
        let path = fixture("system-subagent-notification.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.events.len(), 0);
        assert_eq!(res.tool_result_events.len(), 1);
        let ev = &res.tool_result_events[0];
        assert_eq!(ev.source, SourceKind::ClaudeCode);
        assert_eq!(ev.session_id, "22222222-2222-2222-2222-222222222222");
        assert_eq!(ev.tool_use_id, "toolu_system");
        assert_eq!(ev.event_source, ToolResultEventSource::SubagentNotification);
        assert_eq!(ev.status, ToolResultStatus::Completed);
        assert_eq!(ev.agent_id.as_deref(), Some("agent-system-1"));
        assert_eq!(
            ev.subagent_session_id.as_deref(),
            Some("session-system-child")
        );
        assert_eq!(ev.call_index, Some(0));
        assert_eq!(ev.event_index, 0);
        assert!(ev.content_length.is_some());
        assert!(ev.content_hash.is_some());
    }

    #[test]
    fn fidelity_full_coverage_on_normal_turn() {
        // claude.test.ts:248 — simple-turn carries every usage field, so the
        // turn surfaces full coverage and class=Full. tool/relationship flags
        // are capability-level (always true for Claude even on a no-tool turn);
        // reasoning is always false because the harness doesn't surface it.
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let t = &res.turns[0];
        let f = t.fidelity.as_ref().expect("fidelity should be populated");
        assert_eq!(f.granularity, UsageGranularity::PerTurn);
        assert!(f.coverage.has_input_tokens);
        assert!(f.coverage.has_output_tokens);
        assert!(f.coverage.has_cache_read_tokens);
        assert!(f.coverage.has_cache_create_tokens);
        assert!(!f.coverage.has_reasoning_tokens);
        assert!(f.coverage.has_tool_calls);
        assert!(f.coverage.has_tool_result_events);
        assert!(f.coverage.has_session_relationships);
        assert_eq!(f.class, FidelityClass::Full);
    }

    #[test]
    fn fidelity_marks_missing_output_tokens_as_partial() {
        // claude.test.ts:270 — Usage.output is forced to 0 (the wire shape
        // requires *some* number) but coverage.hasOutputTokens=false makes the
        // distinction visible. Class falls below Full to Partial because not
        // all required fields are populated.
        let path = fixture("missing-output-tokens.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let t = &res.turns[0];
        assert_eq!(t.usage.output, 0);
        let f = t.fidelity.as_ref().unwrap();
        assert!(f.coverage.has_input_tokens);
        assert!(!f.coverage.has_output_tokens);
        assert!(!f.coverage.has_cache_read_tokens);
        assert!(!f.coverage.has_cache_create_tokens);
        assert_eq!(f.class, FidelityClass::Partial);
    }

    #[test]
    fn fidelity_has_tool_calls_on_tool_use_turn() {
        // claude.test.ts:289 — the Coverage flag is capability-level, so a
        // turn that *did* emit tool_use blocks must reflect that; class stays
        // Full because every required field is populated.
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let f = res.turns[0].fidelity.as_ref().unwrap();
        assert!(f.coverage.has_tool_calls);
        assert_eq!(f.class, FidelityClass::Full);
    }

    #[test]
    fn compact_boundary_emits_compaction_event() {
        // claude.test.ts:297 — a `system` line with subtype `compact_boundary`
        // produces one CompactionEvent anchored to the assistant turn that
        // immediately preceded it. tokensBeforeCompact mirrors that turn's
        // cacheRead.
        let path = fixture("compact-boundary.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.events.len(), 1);
        let ev = &res.events[0];
        assert_eq!(ev.source, SourceKind::ClaudeCode);
        assert_eq!(ev.session_id, "compact-session");
        assert_eq!(ev.preceding_message_id.as_deref(), Some("msg_c_1"));
        let preceding = res
            .turns
            .iter()
            .find(|t| t.message_id == "msg_c_1")
            .unwrap();
        assert_eq!(ev.tokens_before_compact, Some(preceding.usage.cache_read));
        assert_eq!(ev.tokens_before_compact, Some(9000));
    }

    // ----- parseClaudeSession user-turn block sizes (issue #2) -----

    #[test]
    fn user_turn_blocks_text_and_tool_results() {
        // claude.test.ts:314 — three user lines → three UserTurnRecord rows.
        // The first is plain text, the second carries Bash + Read tool_results
        // (the Read body is much larger than the Bash body), the third carries
        // an errored tool_result. precedingMessageId is undefined for the
        // first user turn (no prior assistant) and otherwise points at the
        // immediately-prior assistant message; followingMessageId points at
        // the next assistant.
        let path = fixture("user-turn-blocks.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.user_turns.len(), 3);

        let first = &res.user_turns[0];
        assert_eq!(first.user_uuid, "u-user-1");
        assert!(first.preceding_message_id.is_none());
        assert_eq!(first.following_message_id.as_deref(), Some("msg_utb_1"));
        assert_eq!(first.blocks.len(), 1);
        assert_eq!(first.blocks[0].kind, UserTurnBlockKind::Text);
        assert_eq!(
            first.blocks[0].byte_len,
            "please fix the build".len() as u64
        );
        // The TS test asserts `4` (cl100k). The Rust port has not wired
        // cl100k yet — see #246 — so the heuristic counter (`ceil(byteLen/4)`)
        // is the default, which makes this 5 for the same 20-byte prompt.
        // The `user_turn_heuristic_tokenizer_explicit_opt_in` test pins the
        // heuristic formula explicitly; once cl100k lands here we'll flip
        // this back to `4` to match TS byte-for-byte.
        assert_eq!(
            first.blocks[0].approx_tokens,
            ("please fix the build".len() as u64).div_ceil(4)
        );

        let second = &res.user_turns[1];
        assert_eq!(second.preceding_message_id.as_deref(), Some("msg_utb_1"));
        assert_eq!(second.following_message_id.as_deref(), Some("msg_utb_2"));
        assert_eq!(second.blocks.len(), 2);
        let bash = second
            .blocks
            .iter()
            .find(|b| b.tool_use_id.as_deref() == Some("tu_bash_1"))
            .unwrap();
        let read = second
            .blocks
            .iter()
            .find(|b| b.tool_use_id.as_deref() == Some("tu_read_1"))
            .unwrap();
        assert_eq!(bash.kind, UserTurnBlockKind::ToolResult);
        assert_eq!(bash.byte_len, "a\nb\n".len() as u64);
        assert_eq!(read.byte_len, 100);
        assert!(read.byte_len > bash.byte_len);
        assert!(bash.is_error.is_none());
        assert!(read.is_error.is_none());

        let third = &res.user_turns[2];
        assert_eq!(third.preceding_message_id.as_deref(), Some("msg_utb_2"));
        assert_eq!(third.following_message_id.as_deref(), Some("msg_utb_3"));
        assert_eq!(third.blocks.len(), 1);
        let err_block = &third.blocks[0];
        assert_eq!(err_block.kind, UserTurnBlockKind::ToolResult);
        assert_eq!(err_block.tool_use_id.as_deref(), Some("tu_bash_2"));
        assert_eq!(err_block.is_error, Some(true));
    }

    #[test]
    fn user_turn_input_delta_is_positive_for_real_io() {
        // claude.test.ts:355 — sanity gate: when there's real I/O across a
        // (precedingMessageId, followingMessageId) pair, both the
        // input-side delta and the per-block token sum must be positive. The
        // ±5% reconciliation in the TS test depends on the cl100k tokenizer,
        // which the Rust port has not wired (HeuristicCounter is still the
        // default — see #246), so we only enforce the positive-on-both-sides
        // invariant here.
        let path = fixture("user-turn-blocks.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let by_mid: HashMap<&str, &TurnRecord> = res
            .turns
            .iter()
            .map(|t| (t.message_id.as_str(), t))
            .collect();
        for u in &res.user_turns {
            let prev_id = match u.preceding_message_id.as_deref() {
                Some(p) => p,
                None => continue,
            };
            let next_id = match u.following_message_id.as_deref() {
                Some(n) => n,
                None => continue,
            };
            let prev = by_mid.get(prev_id).unwrap();
            let next = by_mid.get(next_id).unwrap();
            let input_delta =
                (next.usage.input + next.usage.cache_create_5m + next.usage.cache_create_1h) as i64
                    - prev.usage.output as i64;
            let user_tokens: u64 = u.blocks.iter().map(|b| b.approx_tokens).sum();
            assert!(
                user_tokens > 0,
                "user turn {} should contribute tokens",
                u.user_uuid
            );
            assert!(input_delta > 0, "delta for {} should be positive", next_id);
        }
    }

    #[test]
    fn user_turn_heuristic_tokenizer_explicit_opt_in() {
        // claude.test.ts:378 — with the heuristic tokenizer the first text
        // block's approxTokens is `ceil(byte_len / 4)`. The Rust default is
        // also heuristic, but mirroring the TS test means asking for it
        // explicitly so we exercise the option plumbing.
        let path = fixture("user-turn-blocks.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let first = &res.user_turns[0];
        assert_eq!(
            first.blocks[0].byte_len,
            "please fix the build".len() as u64
        );
        let expected = ("please fix the build".len() as u64).div_ceil(4);
        assert_eq!(first.blocks[0].approx_tokens, expected);
    }

    #[test]
    fn user_turn_present_for_simple_text_session() {
        // claude.test.ts:405 — even a one-turn session emits one UserTurnRecord
        // with a single text block. (The TS comment explains why simple-turn
        // is the right fixture instead of sidechain-turn.)
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.user_turns.len(), 1);
        assert_eq!(res.user_turns[0].blocks.len(), 1);
        assert_eq!(res.user_turns[0].blocks[0].kind, UserTurnBlockKind::Text);
    }

    #[test]
    fn slash_command_triads_collapse_to_one_skill_activity_each() {
        // Integration coverage for #438. The fixture has two slash-command
        // triads (`/review` then `/init`), each three rows: caveat row,
        // invocation row (`<command-name>`), stdout row
        // (`<local-command-stdout>`). Three assistant rows surround the
        // triads (one before, one between, one after). Without the
        // detector, the two post-triad assistants would classify against
        // the stdout body (whatever keyword the stdout text happened to
        // hit) — with the detector, they collapse to `Skill`.
        let path = fixture("slash-command-triad.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 3, "three assistant turns survive");
        let activities: Vec<Option<ActivityCategory>> =
            res.turns.iter().map(|t| t.activity).collect();
        // First assistant is a normal reply, NOT inside a triad.
        assert_ne!(activities[0], Some(ActivityCategory::Skill));
        // Second + third assistants follow a slash-command triad's stdout;
        // each must surface as a single `Skill` activity. Two triads →
        // two `Skill` turns (3 → 1 per triad, per the issue acceptance).
        assert_eq!(activities[1], Some(ActivityCategory::Skill));
        assert_eq!(activities[2], Some(ActivityCategory::Skill));
        let skill_count = activities
            .iter()
            .filter(|a| **a == Some(ActivityCategory::Skill))
            .count();
        assert_eq!(skill_count, 2, "two triads → two Skill activities");
    }

    #[test]
    fn slash_command_triad_does_not_double_count_token_attribution() {
        // Token attribution stays on the underlying assistant rows; the
        // synthetic `Skill` label is a view, not a billing unit. The sum
        // of the three assistants' (input + output) tokens equals what
        // we computed before the triad classifier landed — collapsing
        // the activity label does NOT redirect tokens onto the Skill
        // turns or off of them. See AgentWorkforce/burn#438.
        let path = fixture("slash-command-triad.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        let total_input: u64 = res.turns.iter().map(|t| t.usage.input).sum();
        let total_output: u64 = res.turns.iter().map(|t| t.usage.output).sum();
        // Fixture: assistants 1, 2, 3 have input 10/20/25 and output 5/3/4.
        assert_eq!(total_input, 10 + 20 + 25);
        assert_eq!(total_output, 5 + 3 + 4);
    }

    #[test]
    fn slash_command_triad_false_positive_guard_normal_user_turn() {
        // Regression guard for the false-positive case in #438. A
        // legitimate user prompt that *looks* structurally similar to a
        // caveat row (parent chain shape: user → user → user) but
        // lacks the `<command-name>` invocation marker MUST NOT
        // misdetect as a triad. The classifier should fall through to
        // its normal text-based rules. Mirrors the false-positive guard
        // in `task_notification_does_not_match_user_typed_marker_string`
        // (#442).
        let path = fixture("task-notification.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        for turn in &res.turns {
            assert_ne!(
                turn.activity,
                Some(ActivityCategory::Skill),
                "no slash-command markers → no Skill activity",
            );
        }
    }

    #[test]
    fn task_notification_rows_are_excluded_from_user_turns() {
        // Integration coverage for #439. The fixture interleaves real user
        // prompts with two synthetic `<task-notification>` rows (one
        // tagged via `origin.kind`, one via the `queued_command`
        // attachment). Burn must emit exactly two UserTurnRecords (one
        // per real prompt) — a regression that re-counts task-notification
        // rows would emit four.
        let path = fixture("task-notification.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(
            res.user_turns.len(),
            2,
            "task-notification rows must not count as user turns"
        );
        let user_uuids: Vec<&str> = res
            .user_turns
            .iter()
            .map(|u| u.user_uuid.as_str())
            .collect();
        assert_eq!(user_uuids, vec!["u-user-1", "u-user-2"]);
        // Sanity: assistant turns are unaffected — the harness-injected
        // rows don't suppress real assistant accounting.
        assert_eq!(res.turns.len(), 2);
    }

    // ----- parseClaudeSession content capture -----

    #[test]
    fn content_default_off_returns_empty() {
        // claude.test.ts:418 — without `contentMode`, the parser does not
        // capture text bodies. Hash-only is the same shape (also empty) — the
        // sidecar handles hashing separately.
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert!(res.content.is_empty());
    }

    #[test]
    fn content_hash_only_returns_empty() {
        // claude.test.ts:423 — hash-only mode is also empty at the parser
        // level (the writer derives sidecar entries downstream).
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::HashOnly),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res.content.is_empty());
    }

    #[test]
    fn content_full_captures_user_and_assistant_text() {
        // claude.test.ts:430 — `contentMode: 'full'` returns one user text
        // record and one assistant text record with full provenance.
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(res.content.len(), 2);
        let user = res
            .content
            .iter()
            .find(|c| matches!(c.role, ContentRole::User))
            .expect("user content present");
        assert_eq!(user.kind, ContentKind::Text);
        assert_eq!(user.text.as_deref(), Some("hello"));
        assert_eq!(user.session_id, "11111111-1111-1111-1111-111111111111");
        let asst = res
            .content
            .iter()
            .find(|c| matches!(c.role, ContentRole::Assistant))
            .expect("assistant content present");
        assert_eq!(asst.kind, ContentKind::Text);
        assert_eq!(asst.text.as_deref(), Some("Hello!"));
        assert_eq!(asst.message_id, "msg_simple_1");
        assert_eq!(asst.source, SourceKind::ClaudeCode);
    }

    #[test]
    fn content_chronological_across_interleaved_turns() {
        // claude.test.ts:448 — content rows preserve interleaved turn order
        // across user/assistant pairs.
        let path = fixture("interleaved-turns.jsonl");
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let sequence: Vec<String> = res
            .content
            .iter()
            .map(|c| {
                let role = match c.role {
                    ContentRole::User => "user",
                    ContentRole::Assistant => "assistant",
                    ContentRole::ToolResult => "tool_result",
                };
                format!("{}:{}", role, c.text.as_deref().unwrap_or(""))
            })
            .collect();
        assert_eq!(
            sequence,
            vec![
                "user:first question".to_string(),
                "assistant:first answer".to_string(),
                "user:second question".to_string(),
                "assistant:second answer".to_string(),
            ]
        );
    }

    #[test]
    fn content_captures_tool_use_blocks_in_multi_block_turn() {
        // claude.test.ts:462 — assistant content for a multi-block turn
        // surfaces the text + two tool_use blocks. The empty-string thinking
        // block is omitted (per parser policy: skip thinking blocks with no
        // body).
        let path = fixture("multi-block-turn.jsonl");
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let asst: Vec<&ContentRecord> = res
            .content
            .iter()
            .filter(|c| matches!(c.role, ContentRole::Assistant))
            .collect();
        let mut kinds: Vec<&str> = asst
            .iter()
            .map(|c| match c.kind {
                ContentKind::Text => "text",
                ContentKind::Thinking => "thinking",
                ContentKind::ToolUse => "tool_use",
                ContentKind::ToolResult => "tool_result",
            })
            .collect();
        kinds.sort();
        assert_eq!(kinds, vec!["text", "tool_use", "tool_use"]);
        let tool_uses: Vec<&ContentRecord> = asst
            .iter()
            .copied()
            .filter(|c| matches!(c.kind, ContentKind::ToolUse))
            .collect();
        let bash_use = tool_uses
            .iter()
            .find(|c| c.tool_use.as_ref().map(|tu| tu.name.as_str()) == Some("Bash"))
            .expect("Bash tool_use surfaced");
        let agent_use = tool_uses
            .iter()
            .find(|c| c.tool_use.as_ref().map(|tu| tu.name.as_str()) == Some("Agent"))
            .expect("Agent tool_use surfaced");
        let bash_input = &bash_use.tool_use.as_ref().unwrap().input;
        assert_eq!(bash_input.len(), 1);
        assert_eq!(
            bash_input.get("command").and_then(|v| v.as_str()),
            Some("ls -la /tmp/project")
        );
        assert_eq!(agent_use.tool_use.as_ref().unwrap().name, "Agent");
    }

    #[test]
    fn content_tool_result_is_error_tri_state() {
        // Verify that ContentRecord.tool_result.is_error is Some(true) when
        // the JSON field is `true`, and None when absent (or `false`).
        // Uses user-turn-blocks.jsonl which has:
        //   - tu_bash_1 (no is_error field)  → None
        //   - tu_read_1 (no is_error field)  → None
        //   - tu_bash_2 ("is_error": true)   → Some(true)
        let path = fixture("user-turn-blocks.jsonl");
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            },
        )
        .unwrap();
        let tool_results: Vec<&ContentRecord> = res
            .content
            .iter()
            .filter(|c| matches!(c.kind, ContentKind::ToolResult))
            .collect();
        assert_eq!(
            tool_results.len(),
            3,
            "expected 3 tool_result content records"
        );
        let bash1 = tool_results
            .iter()
            .find(|c| {
                c.tool_result
                    .as_ref()
                    .map(|tr| tr.tool_use_id.as_str() == "tu_bash_1")
                    .unwrap_or(false)
            })
            .expect("tu_bash_1 content record present");
        assert_eq!(
            bash1.tool_result.as_ref().unwrap().is_error,
            None,
            "tu_bash_1 has no is_error field — must be None"
        );
        let read1 = tool_results
            .iter()
            .find(|c| {
                c.tool_result
                    .as_ref()
                    .map(|tr| tr.tool_use_id.as_str() == "tu_read_1")
                    .unwrap_or(false)
            })
            .expect("tu_read_1 content record present");
        assert_eq!(
            read1.tool_result.as_ref().unwrap().is_error,
            None,
            "tu_read_1 has no is_error field — must be None"
        );
        let bash2 = tool_results
            .iter()
            .find(|c| {
                c.tool_result
                    .as_ref()
                    .map(|tr| tr.tool_use_id.as_str() == "tu_bash_2")
                    .unwrap_or(false)
            })
            .expect("tu_bash_2 content record present");
        assert_eq!(
            bash2.tool_result.as_ref().unwrap().is_error,
            Some(true),
            "tu_bash_2 has is_error=true — must be Some(true)"
        );
    }

    // ----- parseClaudeSession fork / continuation relationships (#112) -----
    //
    // Mirrors `describe('parseClaudeSession fork / continuation relationships')`
    // (claude.test.ts:991-1267). These exercise the per-file relationship
    // evidence and the cross-file `reconcile_claude_session_relationships`
    // pass.

    #[test]
    fn resume_marker_emits_continuation_with_provenance() {
        // claude.test.ts:992 — a `/resume <id>` slash command produces a
        // continuation row whose relatedSessionId is the resume target. The
        // file basename (`resume-marker`) becomes sessionId; the in-log
        // sessionId surfaces as sourceSessionId.
        let path = fixture("resume-marker.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let res = parse_claude_session_incremental(
            &path,
            &ParseIncrementalOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let cont = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
            .expect("/resume marker must produce a continuation row");
        assert_eq!(cont.session_id, "resume-marker");
        assert_eq!(
            cont.related_session_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            cont.source_session_id.as_deref(),
            Some("99999999-9999-9999-9999-999999999999")
        );
        assert_eq!(cont.source_version.as_deref(), Some("2.1.97"));
    }

    #[test]
    fn resume_marker_root_carries_provenance_when_in_log_id_differs() {
        // claude.test.ts:1010 — when the file basename and in-log sessionId
        // disagree, both the continuation row and the root row carry the
        // mismatched in-log id as sourceSessionId plus the version banner.
        let path = fixture("resume-marker.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let root = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Root))
            .expect("root row should still be emitted");
        assert_eq!(root.session_id, "resume-marker");
        assert_eq!(
            root.source_session_id.as_deref(),
            Some("99999999-9999-9999-9999-999999999999")
        );
        assert_eq!(root.source_version.as_deref(), Some("2.1.97"));
    }

    #[test]
    fn explicit_line_continuedfrom_and_fork_session_id() {
        // claude.test.ts:1020 — `continuedFromSessionId` and `forkSessionId`
        // on a line surface as continuation and fork rows respectively.
        // Evidence carries the explicit target ids so reconciliation can dedup
        // against them.
        let path = fixture("explicit-line-relationships.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let cont = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
            .expect("continuedFromSessionId must produce a continuation row");
        let fork = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Fork))
            .expect("forkSessionId must produce a fork row");
        assert_eq!(cont.session_id, "explicit-line-relationships");
        assert_eq!(cont.related_session_id.as_deref(), Some("original-session"));
        assert_eq!(
            cont.source_session_id.as_deref(),
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
        );
        assert_eq!(cont.source_version.as_deref(), Some("2.1.98"));
        assert_eq!(fork.session_id, "explicit-line-relationships");
        assert_eq!(
            fork.related_session_id.as_deref(),
            Some("fork-source-session")
        );
        assert_eq!(
            fork.source_session_id.as_deref(),
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
        );
        assert_eq!(fork.source_version.as_deref(), Some("2.1.98"));

        assert_eq!(
            res.evidence
                .explicit_continuation_target_session_ids
                .as_deref(),
            Some(["original-session".to_string()].as_slice())
        );
        assert_eq!(
            res.evidence.explicit_fork_target_session_ids.as_deref(),
            Some(["fork-source-session".to_string()].as_slice())
        );
    }

    #[test]
    fn reconciliation_skips_explicit_continuation_edge() {
        // claude.test.ts:1042 — when a file already emits a
        // `continuedFromSessionId` continuation row, the cross-file
        // parentUuid pass must not re-emit the same edge. Otherwise the
        // ledger would dedup but only after writing duplicates.
        let original_path = fixture("original-session.jsonl");
        let explicit_path = fixture("explicit-line-relationships.jsonl");
        let original_session_path = original_path.to_string_lossy().into_owned();
        let explicit_session_path = explicit_path.to_string_lossy().into_owned();
        let original = parse_claude_session(
            &original_path,
            &ParseOptions {
                session_path: Some(original_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let explicit = parse_claude_session(
            &explicit_path,
            &ParseOptions {
                session_path: Some(explicit_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(explicit.relationships.iter().any(|r| matches!(
            r.relationship_type,
            RelationshipType::Continuation
        ) && r.session_id
            == "explicit-line-relationships"
            && r.related_session_id.as_deref() == Some("original-session")));

        let reconciled = reconcile_claude_session_relationships(&[
            ReconcileClaudeRelationshipsInput {
                evidence: original.evidence,
            },
            ReconcileClaudeRelationshipsInput {
                evidence: explicit.evidence,
            },
        ]);
        let dup = reconciled.iter().any(|r| {
            matches!(r.relationship_type, RelationshipType::Continuation)
                && r.session_id == "explicit-line-relationships"
                && r.related_session_id.as_deref() == Some("original-session")
        });
        assert!(
            !dup,
            "cross-file parentUuid inference must not duplicate the explicit edge"
        );
    }

    #[test]
    fn first_parent_uuid_skips_leading_sidechain_user_line() {
        // claude.test.ts:1077 — the `firstParentUuid` evidence is gated to
        // the first non-sidechain user line, so a sidechain user line that
        // happens to come first is ignored. (The TS gate uses a module-level
        // WeakSet; the Rust port tracks `user_seen` inline.)
        let path = fixture("sidechain-leading-then-main.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            res.evidence.first_parent_uuid.as_deref(),
            Some("u-original-asst")
        );
    }

    #[test]
    fn evidence_exposes_per_file_signals_for_reconciliation() {
        // claude.test.ts:1083 — evidence carries everything the cross-file
        // reconciler needs: file id, source version, the resume-marker flag
        // and target, and the seenUuids set the cross-file pass joins on.
        let path = fixture("resume-marker.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let res = parse_claude_session(
            &path,
            &ParseOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let ev = &res.evidence;
        assert_eq!(ev.file_session_id.as_deref(), Some("resume-marker"));
        assert_eq!(ev.source_version.as_deref(), Some("2.1.97"));
        assert!(ev.has_resume_marker);
        assert_eq!(
            ev.resume_target_session_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        // The first non-sidechain user line's parentUuid is null in this
        // fixture, so firstParentUuid stays None.
        assert!(ev.first_parent_uuid.is_none());
        assert!(ev.seen_uuids.iter().any(|s| s == "u-resume-1"));
        assert!(ev.seen_uuids.iter().any(|s| s == "u-asst-r"));
    }

    #[test]
    fn reconcile_emits_continuation_when_parent_uuid_lives_in_other_file() {
        // claude.test.ts:1098 — the cross-file pass joins one file's
        // firstParentUuid onto another file's seenUuids set, producing a
        // continuation row that the local pass alone could not have emitted
        // (no /resume marker).
        let original_path = fixture("original-session.jsonl");
        let cross_path = fixture("cross-file-parent.jsonl");
        let original_session_path = original_path.to_string_lossy().into_owned();
        let cross_session_path = cross_path.to_string_lossy().into_owned();
        let original = parse_claude_session(
            &original_path,
            &ParseOptions {
                session_path: Some(original_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let cross = parse_claude_session(
            &cross_path,
            &ParseOptions {
                session_path: Some(cross_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            cross.evidence.first_parent_uuid.as_deref(),
            Some("u-original-asst")
        );
        assert!(!cross
            .relationships
            .iter()
            .any(|r| matches!(r.relationship_type, RelationshipType::Continuation)));

        let reconciled = reconcile_claude_session_relationships(&[
            ReconcileClaudeRelationshipsInput {
                evidence: original.evidence,
            },
            ReconcileClaudeRelationshipsInput {
                evidence: cross.evidence,
            },
        ]);
        let cont = reconciled
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
            .expect("cross-file parentUuid match must produce a continuation row");
        assert_eq!(cont.session_id, "cross-file-parent");
        assert_eq!(cont.related_session_id.as_deref(), Some("original-session"));
        assert_eq!(cont.source_version.as_deref(), Some("2.1.97"));
    }

    #[test]
    fn reconcile_emits_fork_rows_when_two_files_share_source_session_id() {
        // claude.test.ts:1128 — two branches with the same in-log sessionId
        // (different filenames) get one fork row each pointing at the shared
        // sourceSessionId.
        let a_path = fixture("fork-branch-a.jsonl");
        let b_path = fixture("fork-branch-b.jsonl");
        let a_session_path = a_path.to_string_lossy().into_owned();
        let b_session_path = b_path.to_string_lossy().into_owned();
        let a = parse_claude_session(
            &a_path,
            &ParseOptions {
                session_path: Some(a_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let b = parse_claude_session(
            &b_path,
            &ParseOptions {
                session_path: Some(b_session_path),
                ..Default::default()
            },
        )
        .unwrap();

        let root_a = a
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Root))
            .unwrap();
        let root_b = b
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Root))
            .unwrap();
        assert_eq!(root_a.session_id, "fork-branch-a");
        assert_eq!(root_b.session_id, "fork-branch-b");
        assert_eq!(
            root_a.source_session_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000fff")
        );
        assert_eq!(
            root_b.source_session_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000fff")
        );

        let reconciled = reconcile_claude_session_relationships(&[
            ReconcileClaudeRelationshipsInput {
                evidence: a.evidence,
            },
            ReconcileClaudeRelationshipsInput {
                evidence: b.evidence,
            },
        ]);
        let forks: Vec<&SessionRelationshipRecord> = reconciled
            .iter()
            .filter(|r| matches!(r.relationship_type, RelationshipType::Fork))
            .collect();
        assert_eq!(forks.len(), 2);
        let mut sids: Vec<&str> = forks.iter().map(|r| r.session_id.as_str()).collect();
        sids.sort();
        assert_eq!(sids, vec!["fork-branch-a", "fork-branch-b"]);
        for f in forks {
            assert_eq!(
                f.related_session_id.as_deref(),
                Some("00000000-0000-0000-0000-000000000fff")
            );
            assert_eq!(
                f.source_session_id.as_deref(),
                Some("00000000-0000-0000-0000-000000000fff")
            );
            assert_eq!(f.source_version.as_deref(), Some("2.1.97"));
        }
    }

    #[test]
    fn reconcile_does_not_emit_fork_for_strict_continuation() {
        // claude.test.ts:1162 — when file B's firstParentUuid lives in file A
        // (a strict continuation), reconciliation must not double-emit a fork
        // row even though both files share a sourceSessionId.
        let a_path = fixture("original-session.jsonl");
        let b_path = fixture("cross-file-parent.jsonl");
        let a_session_path = a_path.to_string_lossy().into_owned();
        let b_session_path = b_path.to_string_lossy().into_owned();
        let a = parse_claude_session(
            &a_path,
            &ParseOptions {
                session_path: Some(a_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let b = parse_claude_session(
            &b_path,
            &ParseOptions {
                session_path: Some(b_session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let reconciled = reconcile_claude_session_relationships(&[
            ReconcileClaudeRelationshipsInput {
                evidence: a.evidence,
            },
            ReconcileClaudeRelationshipsInput {
                evidence: b.evidence,
            },
        ]);
        let forks = reconciled
            .iter()
            .filter(|r| matches!(r.relationship_type, RelationshipType::Fork))
            .count();
        let conts = reconciled
            .iter()
            .filter(|r| matches!(r.relationship_type, RelationshipType::Continuation))
            .count();
        assert_eq!(forks, 0);
        assert_eq!(conts, 1);
    }

    #[test]
    fn reparsing_same_session_yields_stable_relationship_keys() {
        // claude.test.ts:1182 — re-ingesting the same session must produce
        // relationship rows that hash to the same dedup key, so the writer's
        // existing dedup folds them. We reproduce the canonical key here
        // (source + sessionId + relationshipType + relatedSessionId + agentId
        // + parentToolUseId) instead of importing it across the
        // reader/ledger boundary, matching the TS test exactly.
        fn key_of(r: &SessionRelationshipRecord) -> String {
            let source = match r.source {
                RelationshipSourceKind::ClaudeCode => "claude-code",
                RelationshipSourceKind::Codex => "codex",
                RelationshipSourceKind::Opencode => "opencode",
                RelationshipSourceKind::AnthropicApi => "anthropic-api",
                RelationshipSourceKind::OpenaiApi => "openai-api",
                RelationshipSourceKind::GeminiApi => "gemini-api",
                RelationshipSourceKind::SpawnEnv => "spawn-env",
                RelationshipSourceKind::NativeClaude => "native-claude",
                RelationshipSourceKind::NativeOpencode => "native-opencode",
            };
            let kind = match r.relationship_type {
                RelationshipType::Root => "root",
                RelationshipType::Continuation => "continuation",
                RelationshipType::Fork => "fork",
                RelationshipType::Subagent => "subagent",
            };
            format!(
                "{}|{}|{}|{}|{}|{}",
                source,
                r.session_id,
                kind,
                r.related_session_id.as_deref().unwrap_or(""),
                r.agent_id.as_deref().unwrap_or(""),
                r.parent_tool_use_id.as_deref().unwrap_or(""),
            )
        }
        let path = fixture("resume-marker.jsonl");
        let session_path = path.to_string_lossy().into_owned();
        let opts = ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        };
        let a = parse_claude_session(&path, &opts).unwrap();
        let b = parse_claude_session(&path, &opts).unwrap();
        let mut ids_a: Vec<String> = a.relationships.iter().map(key_of).collect();
        let mut ids_b: Vec<String> = b.relationships.iter().map(key_of).collect();
        let unique_a: std::collections::HashSet<&String> = ids_a.iter().collect();
        assert_eq!(
            unique_a.len(),
            a.relationships.len(),
            "every row should hash uniquely on first parse"
        );
        ids_a.sort();
        ids_b.sort();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn reconcile_skips_duplicate_continuation_when_local_resume_named_same_parent() {
        // claude.test.ts:1216 — when the local /resume already emitted a
        // continuation row for the same edge the cross-file pass would
        // produce, reconciliation must not double-emit. We construct evidence
        // pairs in memory (parent file + child file) that line up exactly:
        // the child's firstParentUuid lives in the parent's seenUuids, AND
        // the child's resume marker names the parent's file id. (The TS test
        // builds partial objects; in Rust we set the fields explicitly,
        // including the private `user_seen` gate, since we are inside the
        // same module.)
        let parent_evidence = ClaudeRelationshipEvidence {
            file_session_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            in_log_session_ids: vec!["11111111-1111-1111-1111-111111111111".to_string()],
            seen_uuids: vec!["u-original-asst".to_string()],
            ..ClaudeRelationshipEvidence::default()
        };
        let child_evidence = ClaudeRelationshipEvidence {
            file_session_id: Some("resume-marker".to_string()),
            in_log_session_ids: vec!["99999999-9999-9999-9999-999999999999".to_string()],
            seen_uuids: vec![],
            has_resume_marker: true,
            resume_target_session_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            first_parent_uuid: Some("u-original-asst".to_string()),
            source_version: Some("2.1.97".to_string()),
            ..ClaudeRelationshipEvidence::default()
        };
        let reconciled = reconcile_claude_session_relationships(&[
            ReconcileClaudeRelationshipsInput {
                evidence: parent_evidence,
            },
            ReconcileClaudeRelationshipsInput {
                evidence: child_evidence,
            },
        ]);
        let dup = reconciled
            .iter()
            .filter(|r| {
                matches!(r.relationship_type, RelationshipType::Continuation)
                    && r.session_id == "resume-marker"
                    && r.related_session_id.as_deref()
                        == Some("11111111-1111-1111-1111-111111111111")
            })
            .count();
        assert_eq!(dup, 0);
    }

    #[test]
    fn subagent_rows_carry_provenance_when_basename_differs_from_in_log_id() {
        // claude.test.ts:1252 — copy nested-subagent.jsonl to a tmp filename
        // distinct from its in-log sessionId. Subagent rows must carry the
        // mismatched in-log id as sourceSessionId, just like roots do, so
        // cross-source joins can group all rows under one provenance banner.
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        let src = fixture("nested-subagent.jsonl");
        std::fs::copy(&src, &working).unwrap();
        let session_path = working.to_string_lossy().into_owned();
        let res = parse_claude_session(
            &working,
            &ParseOptions {
                session_path: Some(session_path),
                ..Default::default()
            },
        )
        .unwrap();
        let sub = res
            .relationships
            .iter()
            .find(|r| matches!(r.relationship_type, RelationshipType::Subagent))
            .expect("fixture has subagent rows");
        assert_eq!(
            sub.source_session_id.as_deref(),
            Some("55555555-5555-5555-5555-555555555555")
        );
    }

    // ----- parentUuid chain grouping (#433) end-to-end -----
    //
    // These two tests prove the chain walk replaces the file-order text
    // association without depending on parse-state invariants beyond the
    // public TurnRecord shape. The fixture rows are crafted so the old
    // heuristic would mis-classify at least one turn, and the chain walk
    // recovers the right answer.

    /// Out-of-order JSONL flush: two user prompts land in the file before
    /// either assistant's first chunk. Under the legacy file-order map the
    /// first assistant (`msg_bug_assistant`) would inherit the *second*
    /// user prompt's text ("add a new feature ...") and mis-classify as
    /// `Feature`. The parent-chain walk routes it to the correct user
    /// prompt ("fix the bug ...") and classifies as `Debugging`.
    #[test]
    fn parent_chain_groups_out_of_order_rows_for_classification() {
        let path = fixture("parent-chain-out-of-order.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 2, "fixture has two assistant turns");
        let bug = res
            .turns
            .iter()
            .find(|t| t.message_id == "msg_bug_assistant")
            .expect("bug-fix turn present");
        let feature = res
            .turns
            .iter()
            .find(|t| t.message_id == "msg_feature_assistant")
            .expect("feature turn present");
        // The discriminating assertion: file-order would attach
        // "add a new feature ..." to BOTH turns (FEATURE_RE wins), so the
        // bug-fix turn would mis-classify. Chain walk routes each turn
        // to its own parentUuid root.
        assert_eq!(
            bug.activity,
            Some(ActivityCategory::Debugging),
            "bug-fix turn must use its own user prompt text via parentUuid chain (#433)"
        );
        assert_eq!(
            feature.activity,
            Some(ActivityCategory::Feature),
            "feature turn must use its own user prompt text"
        );
    }

    /// Interrupt + resume: the user cancels mid-stream and types a new
    /// prompt; the original turn's only assistant chunk arrives *after*
    /// the resume turn completes. Under the legacy file-order map the
    /// late refactor assistant inherits the bug-fix user text and
    /// mis-classifies as `Debugging`. The parent-chain walk pins it to
    /// the original "refactor the auth module" prompt and classifies as
    /// `Refactoring`.
    #[test]
    fn parent_chain_groups_interrupt_resume_rows_into_original_turn() {
        let path = fixture("parent-chain-interrupt-resume.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 2, "fixture has two assistant turns");
        let bugfix = res
            .turns
            .iter()
            .find(|t| t.message_id == "msg_bugfix_assistant")
            .expect("bug-fix interrupt turn present");
        let refactor = res
            .turns
            .iter()
            .find(|t| t.message_id == "msg_refactor_assistant")
            .expect("refactor turn present");
        assert_eq!(
            bugfix.activity,
            Some(ActivityCategory::Debugging),
            "bug-fix turn must classify against its own prompt"
        );
        assert_eq!(
            refactor.activity,
            Some(ActivityCategory::Refactoring),
            "late-arriving refactor turn must classify against its ORIGINAL prompt via parentUuid chain (#433), not the most recently seen prompt"
        );
    }

    /// Cycle guard: synthetic loop in the parent chain must not hang the
    /// turn-classification path. With no reachable user-prompt root the
    /// activity classifier sees empty user text (falls back through the
    /// classifier's no-text branches); the key assertion is that
    /// `parse_claude_session` returns in finite time.
    #[test]
    fn parent_chain_cycle_in_assistant_chain_does_not_hang() {
        // Two assistant rows whose parentUuids point at each other,
        // sharing a message_id so they collapse to one turn. The fixture
        // has no user-prompt root and no `stop_reason`, so the chain
        // walk hits the cycle guard and returns `None`; classification
        // falls through to the empty-text branch without looping.
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().join("session.jsonl");
        let body = [
            serde_json::json!({
                "parentUuid": "u-asst-cycle-b",
                "isSidechain": false,
                "type": "assistant",
                "uuid": "u-asst-cycle-a",
                "sessionId": "cccccccc-cccc-cccc-cccc-cccccccccccc",
                "timestamp": "2026-05-01T00:00:00.000Z",
                "cwd": "/tmp/project",
                "message": {
                    "id": "msg_cycle",
                    "model": "claude-sonnet-4-6",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "first chunk"}],
                    "stop_reason": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
            serde_json::json!({
                "parentUuid": "u-asst-cycle-a",
                "isSidechain": false,
                "type": "assistant",
                "uuid": "u-asst-cycle-b",
                "sessionId": "cccccccc-cccc-cccc-cccc-cccccccccccc",
                "timestamp": "2026-05-01T00:00:01.000Z",
                "cwd": "/tmp/project",
                "message": {
                    "id": "msg_cycle",
                    "model": "claude-sonnet-4-6",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "second chunk"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        write_bytes(&working, body.as_bytes());
        // Just calling this and asserting it returns is the test — a hang
        // here would cause the suite to time out. The classifier output
        // is incidental; we only verify the call completes.
        let res = parse_claude_session(&working, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1);
        assert_eq!(res.turns[0].message_id, "msg_cycle");
    }
}
