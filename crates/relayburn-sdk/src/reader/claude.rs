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
//!
//! ## Module layout
//!
//! This file holds the parse engine (line ingest, the `parentUuid` prescan,
//! and the incremental walk in `run_incremental`) plus the small `Value`
//! accessors the submodules share. Cohesive concerns live alongside it:
//!
//! - `parent_chain` — user→assistant `parentUuid` chain walk.
//! - `relationships` — explicit/inferred session relationship reconstruction.
//! - `tool_results` — `tool_result_event` extraction and replacement metadata.
//! - `subagents` — Task sidecar transcript discovery + pairing.
//! - `span_tree` — per-turn span tree projection.
//! - `tests` — conformance tests over `tests/fixtures/claude/*.jsonl`.

mod parent_chain;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use self::parent_chain::ChainNode;
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::hash::{args_hash, content_hash};
use crate::reader::inference::RequestIdLookup;
use crate::reader::types::{
    ActivityCategory, CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode,
    ContentToolResult, ContentToolUse, Coverage, Fidelity, SessionRelationshipRecord, SourceKind,
    Subagent, ToolCall, ToolResultEventRecord, TurnRecord, Usage, UsageGranularity, UserTurnBlock,
    UserTurnRecord,
};
use crate::reader::user_turn::{HeuristicCounter, TokenCounter};

// Re-exported into the conformance test module via its `use super::*;`. The
// production parse engine that referenced these directly now lives in the
// `incremental` submodule, so the root only needs them under `cfg(test)`.
#[cfg(test)]
use crate::reader::types::{RelationshipSourceKind, RelationshipType, StopReason};

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

// Session relationship inference (explicit fork/continuation, inferred
// parent-UUID continuations and shared-source forks, `/resume` markers,
// subagent-spawn and compaction annotation). Split out of this file; the
// parse engine below drives these helpers.
mod relationships;

pub use self::relationships::{
    reconcile_claude_session_relationships, ClaudeRelationshipEvidence,
    ReconcileClaudeRelationshipsInput,
};

// Tool-result event extraction (tool_result blocks, system subagent
// notifications, payload measurement, replacement metadata). Split out of
// this file; the parse engine below drives these helpers per line.
mod tool_results;

use self::tool_results::ReplacementMeta;

// Incremental parse engine: the resume prescan, the `ClaudeParseState`
// streaming state machine, and the `run_incremental` driver the public
// `parse_claude_session*` entry points wrap. Split out of this file; the
// helpers/types above feed it and it drives the relationships/tool_results
// helpers per line.
mod incremental;

use self::incremental::run_incremental;

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
pub(in crate::reader::claude) struct UsageCoverage {
    has_input_tokens: bool,
    has_output_tokens: bool,
    has_cache_read_tokens: bool,
    has_cache_create_tokens: bool,
}

#[derive(Debug, Clone)]
pub(in crate::reader::claude) struct WorkingRecord {
    pub(in crate::reader::claude) message_id: String,
    pub(in crate::reader::claude) first_ts: String,
    pub(in crate::reader::claude) model: String,
    pub(in crate::reader::claude) session_id: String,
    pub(in crate::reader::claude) cwd: Option<String>,
    is_sidechain: bool,
    pub(in crate::reader::claude) usage: Usage,
    pub(in crate::reader::claude) usage_coverage: UsageCoverage,
    pub(in crate::reader::claude) blocks: Vec<Value>,
    pub(in crate::reader::claude) stop_reason: Option<String>,
    first_assistant_uuid: Option<String>,
    #[allow(dead_code)]
    parent_assistant_uuid: Option<String>,
    /// Upstream `requestId` field from the first row that carried one
    /// for this `message_id`. Powers the `Inference` aggregate's
    /// per-API-call key (see `reader/inference.rs` and issue #434).
    /// `None` when no row in this group emitted one — the inference
    /// builder falls back to `message_id`.
    pub(in crate::reader::claude) request_id: Option<String>,
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
pub(in crate::reader::claude) struct LineNode {
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
pub(in crate::reader::claude) struct InvocationInfo {
    root_uuid: String,
    parent_tool_use_id: Option<String>,
    subagent_type: Option<String>,
    description: Option<String>,
    parent_agent_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Line ingest helpers.
// ---------------------------------------------------------------------------

pub(in crate::reader::claude) fn ingest_assistant_record(
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

pub(in crate::reader::claude) fn register_assistant_node(
    line: &Value,
    nodes: &mut HashMap<String, LineNode>,
) {
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

pub(in crate::reader::claude) fn register_user_node(
    line: &Value,
    nodes: &mut HashMap<String, LineNode>,
    is_user_prompt: bool,
) {
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

pub(in crate::reader::claude) fn build_claude_fidelity(uc: &UsageCoverage) -> Fidelity {
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

pub(in crate::reader::claude) fn extract_tool_calls(
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

pub(in crate::reader::claude) fn extract_files_touched(tool_calls: &[ToolCall]) -> Vec<String> {
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

pub(in crate::reader::claude) fn resolve_subagent(
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

pub(in crate::reader::claude) fn extract_assistant_content(
    w: &WorkingRecord,
) -> Vec<ContentRecord> {
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

pub(in crate::reader::claude) fn extract_user_content(
    line: &serde_json::Map<String, Value>,
) -> Vec<ContentRecord> {
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

pub(in crate::reader::claude) fn build_user_turn_record<C: TokenCounter + ?Sized>(
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
// Plain text / errored helpers.
// ---------------------------------------------------------------------------

pub(super) fn extract_plain_user_text_from_obj(
    line: &serde_json::Map<String, Value>,
) -> Option<String> {
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

pub(in crate::reader::claude) fn collect_errored_tool_use_ids(
    line: &serde_json::Map<String, Value>,
    into: &mut HashSet<String>,
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
        if bo.get("is_error").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if let Some(id) = bo.get("tool_use_id").and_then(Value::as_str) {
            into.insert(id.to_string());
        }
    }
}

pub(in crate::reader::claude) fn apply_classification(
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
// Misc helpers.
// ---------------------------------------------------------------------------

pub(super) const SESSION_ID_KEYS: &[&str] = &["sessionId", "session_id"];
pub(super) const TIMESTAMP_KEYS: &[&str] = &["timestamp", "ts"];
pub(super) const SOURCE_VERSION_KEYS: &[&str] = &["version", "sourceVersion", "source_version"];

pub(super) fn string_field(
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

pub(super) fn first_present<'a>(
    obj: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Value> {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests;
