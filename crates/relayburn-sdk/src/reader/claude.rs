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
    ToolResultEventRecord, TurnRecord, Usage, UsageGranularity, UserTurnBlock, UserTurnRecord,
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

// Session relationship inference (explicit fork/continuation, inferred
// parent-UUID continuations and shared-source forks, `/resume` markers,
// subagent-spawn and compaction annotation). Split out of this file; the
// parse engine below drives these helpers.
mod relationships;

use self::relationships::{
    annotate_compaction_events, annotate_relationships_with_evidence, annotate_spawn_events,
    collect_explicit_claude_relationships_incremental, collect_subagent_relationships,
    derive_file_session_id_from_parts, emit_local_continuation_from_resume, new_evidence,
    record_evidence_from_line, record_explicit_relationship_evidence, record_resume_marker,
    RelationshipKey,
};
pub use self::relationships::{
    reconcile_claude_session_relationships, ClaudeRelationshipEvidence,
    ReconcileClaudeRelationshipsInput,
};

// Tool-result event extraction (tool_result blocks, system subagent
// notifications, payload measurement, replacement metadata). Split out of
// this file; the parse engine below drives these helpers per line.
mod tool_results;

use self::tool_results::{
    build_claude_system_tool_result_event, collect_replacement_meta, collect_tool_result_events,
    ReplacementMeta,
};

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

/// Owns the mutable working state threaded through the Claude incremental
/// parser's streaming loop, mirroring the codex decomposition's
/// `CodexParseState`. The driver (`run_incremental`) constructs one, dispatches
/// each parsed line to `handle_assistant` / `handle_user` / `handle_system`,
/// then assembles the result from this state's accumulated fields after the
/// loop closes. Field names and initializers reproduce the prior inline locals
/// verbatim.
struct ClaudeParseState {
    file_session_id: Option<String>,
    evidence: ClaudeRelationshipEvidence,
    nodes_by_uuid: HashMap<String, LineNode>,
    invocation_cache: HashMap<String, Option<InvocationInfo>>,
    tool_result_counters: HashMap<String, u64>,
    next_event_index: u64,
    last_assistant_message_id: Option<String>,
    // -1 sentinel: resume marker came from the prescan (definitely emit).
    // u64::MAX sentinel: no resume marker yet seen.
    // Otherwise: byte offset of the line that first set the marker on this pass.
    resume_marker_offset: u64,
    current_user_text: String,
    working: HashMap<String, WorkingRecord>,
    order: Vec<String>,
    message_id_first_offset: HashMap<String, u64>,
    // Legacy file-order map kept as a fallback when the assistant row
    // lacks a `uuid` or its parent chain doesn't terminate at a known
    // user prompt. The preferred lookup is `user_text_by_uuid` walked
    // via `nearest_user_prompt_root` (#433).
    user_text_by_message_id: HashMap<String, String>,
    // User-prompt text keyed by the user line's own `uuid` — read by the
    // parent-chain walker during turn classification. Populated only for
    // real user prompts (task notifications excluded; empty bodies
    // excluded).
    user_text_by_uuid: HashMap<String, String>,
    errored_tool_use_ids: HashSet<String>,
    replacement_meta_by_tool_use_id: HashMap<String, ReplacementMeta>,
    // Slash-command triad detection (#438) needs a flat slice of user-typed
    // rows to look up the parent-UUID chain shape. We accumulate only the
    // minimal field set the detector reads (`type`, `uuid`, `parentUuid`,
    // `message.content`) so memory stays bounded — three rows per triad,
    // and only user-typed rows are stored. Detection runs once after the
    // streaming loop closes; the resulting `skill_uuids` set is consulted
    // by `apply_classification` to override the activity to `Skill`.
    user_rows_for_triad: Vec<serde_json::Map<String, Value>>,
    events: Vec<(u64, CompactionEvent)>,
    pending_user_content: Vec<(u64, ContentRecord)>,
    pending_tool_result_events: Vec<(u64, ToolResultEventRecord)>,
    pending_relationships: Vec<(u64, SessionRelationshipRecord)>,
    pending_user_turns: Vec<(u64, UserTurnRecord)>,
    seen_root_session_ids: HashSet<String>,
    seen_explicit_relationship_ids: HashSet<RelationshipKey>,
    pending_user_turn_inc_idx: Option<usize>,
}

impl ClaudeParseState {
    /// Initialize working state, reproducing the prior inline initializers
    /// verbatim — including the `prescan_nodes` wiring (run only when
    /// `start_offset > 0`) and the `resume_marker_offset` sentinel derivation
    /// off `evidence.has_resume_marker`.
    fn new(
        path: &Path,
        start_offset: u64,
        file_session_id: Option<String>,
        last_user_text: Option<&str>,
    ) -> std::io::Result<Self> {
        let mut evidence = new_evidence(file_session_id.clone());

        let mut nodes_by_uuid: HashMap<String, LineNode> = HashMap::new();
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
        let resume_marker_offset: u64 = if evidence.has_resume_marker {
            0
        } else {
            u64::MAX
        };

        Ok(Self {
            file_session_id,
            evidence,
            nodes_by_uuid,
            invocation_cache: HashMap::new(),
            tool_result_counters,
            next_event_index,
            last_assistant_message_id,
            resume_marker_offset,
            current_user_text: last_user_text.map(str::to_string).unwrap_or_default(),
            working: HashMap::new(),
            order: Vec::new(),
            message_id_first_offset: HashMap::new(),
            user_text_by_message_id: HashMap::new(),
            user_text_by_uuid: HashMap::new(),
            errored_tool_use_ids: HashSet::new(),
            replacement_meta_by_tool_use_id: HashMap::new(),
            user_rows_for_triad: Vec::new(),
            events: Vec::new(),
            pending_user_content: Vec::new(),
            pending_tool_result_events: Vec::new(),
            pending_relationships: Vec::new(),
            pending_user_turns: Vec::new(),
            seen_root_session_ids: HashSet::new(),
            seen_explicit_relationship_ids: HashSet::new(),
            pending_user_turn_inc_idx: None,
        })
    }

    fn handle_assistant(
        &mut self,
        parsed: &Value,
        obj: &serde_json::Map<String, Value>,
        line_start_offset: u64,
    ) {
        let mid = obj
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref mid_str) = mid {
            if let Some(idx) = self.pending_user_turn_inc_idx {
                if !self.message_id_first_offset.contains_key(mid_str) {
                    self.pending_user_turns[idx].1.following_message_id = Some(mid_str.clone());
                    self.pending_user_turn_inc_idx = None;
                }
            }
            self.message_id_first_offset
                .entry(mid_str.clone())
                .or_insert(line_start_offset);
            self.user_text_by_message_id
                .entry(mid_str.clone())
                .or_insert_with(|| self.current_user_text.clone());
            self.last_assistant_message_id = Some(mid_str.clone());
        }
        let session_id = string_field(obj, SESSION_ID_KEYS, false);
        let timestamp = string_field(obj, TIMESTAMP_KEYS, false);
        if let Some(ref sid) = session_id {
            if !sid.is_empty() {
                record_root_incremental(
                    &mut self.pending_relationships,
                    &mut self.seen_root_session_ids,
                    sid,
                    timestamp.as_deref(),
                    line_start_offset,
                    self.file_session_id.as_deref(),
                );
                collect_explicit_claude_relationships_incremental(
                    obj,
                    &mut self.evidence,
                    &mut self.pending_relationships,
                    &mut self.seen_explicit_relationship_ids,
                    self.file_session_id.as_deref().unwrap_or(sid.as_str()),
                    timestamp.as_deref(),
                    line_start_offset,
                );
            }
        }
        record_evidence_from_line(&mut self.evidence, parsed);
        ingest_assistant_record(
            parsed,
            obj,
            &mut self.working,
            &mut self.order,
            &mut self.nodes_by_uuid,
        );
    }

    fn handle_user<C: TokenCounter + ?Sized>(
        &mut self,
        parsed: &Value,
        obj: &serde_json::Map<String, Value>,
        line_start_offset: u64,
        counter: &C,
        capture_content: bool,
    ) {
        // Slash-command triad detector (#438) keeps a slim copy of
        // every user-typed row so a post-loop pass can find the
        // caveat → invocation → stdout chain shape. We clone the
        // row before the rest of the branch consumes it; the
        // detector only reads four fields so memory stays modest
        // (one entry per user row, dropped at function exit).
        self.user_rows_for_triad.push(obj.clone());
        // Harness-injected `<task-notification>` rows share the user
        // envelope but represent system events, not real prompts.
        // Detecting them here keeps them out of `current_user_text`
        // (so the classifier doesn't get task-notification text as
        // "user intent") and out of `pending_user_turns` (so
        // user-turn aggregates aren't inflated). Side effects like
        // session-relationship discovery still run because those
        // are independent of "is this a real user prompt". See
        // AgentWorkforce/burn#439.
        let task_notification = is_task_notification(obj);
        let user_text = if task_notification {
            None
        } else {
            extract_plain_user_text_from_obj(obj).filter(|s| !s.is_empty())
        };
        let is_user_prompt = user_text.is_some();
        register_user_node(parsed, &mut self.nodes_by_uuid, is_user_prompt);
        if let Some(ref text) = user_text {
            self.current_user_text = text.clone();
            // Index by the user line's UUID for the parent-chain
            // walker (#433). Falls back to no-op when the row
            // lacks a `uuid`, in which case file-order remains
            // the only association mechanism for downstream
            // assistants.
            if let Some(uuid) = obj.get("uuid").and_then(Value::as_str) {
                if !uuid.is_empty() {
                    self.user_text_by_uuid
                        .entry(uuid.to_string())
                        .or_insert_with(|| text.clone());
                }
            }
        }
        collect_errored_tool_use_ids(obj, &mut self.errored_tool_use_ids);
        collect_replacement_meta(obj, &mut self.replacement_meta_by_tool_use_id);
        let session_id = string_field(obj, SESSION_ID_KEYS, false);
        let timestamp = string_field(obj, TIMESTAMP_KEYS, false);
        if let Some(ref sid) = session_id {
            if !sid.is_empty() {
                record_root_incremental(
                    &mut self.pending_relationships,
                    &mut self.seen_root_session_ids,
                    sid,
                    timestamp.as_deref(),
                    line_start_offset,
                    self.file_session_id.as_deref(),
                );
                collect_explicit_claude_relationships_incremental(
                    obj,
                    &mut self.evidence,
                    &mut self.pending_relationships,
                    &mut self.seen_explicit_relationship_ids,
                    self.file_session_id.as_deref().unwrap_or(sid.as_str()),
                    timestamp.as_deref(),
                    line_start_offset,
                );
            }
        }
        record_evidence_from_line(&mut self.evidence, parsed);
        let had_resume_before = self.evidence.has_resume_marker;
        record_resume_marker(&mut self.evidence, obj);
        if !had_resume_before && self.evidence.has_resume_marker {
            self.resume_marker_offset = line_start_offset;
        }
        let mut harvested: Vec<ToolResultEventRecord> = Vec::new();
        self.next_event_index = collect_tool_result_events(
            obj,
            &mut harvested,
            &mut self.tool_result_counters,
            self.next_event_index,
        );
        for ev in harvested {
            self.pending_tool_result_events
                .push((line_start_offset, ev));
        }
        if !task_notification {
            if let Some(record) =
                build_user_turn_record(obj, self.last_assistant_message_id.as_deref(), counter)
            {
                let idx = self.pending_user_turns.len();
                self.pending_user_turns.push((line_start_offset, record));
                self.pending_user_turn_inc_idx = Some(idx);
            }
        }
        if capture_content {
            for c in extract_user_content(obj) {
                self.pending_user_content.push((line_start_offset, c));
            }
        }
    }

    fn handle_system(&mut self, obj: &serde_json::Map<String, Value>, line_start_offset: u64) {
        if obj.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            let session_id = string_field(obj, SESSION_ID_KEYS, false).unwrap_or_default();
            let ts = string_field(obj, TIMESTAMP_KEYS, false).unwrap_or_default();
            if !session_id.is_empty() {
                let mut ev = CompactionEvent {
                    v: 1,
                    source: SourceKind::ClaudeCode,
                    session_id,
                    ts,
                    preceding_message_id: None,
                    tokens_before_compact: None,
                };
                if let Some(ref last) = self.last_assistant_message_id {
                    ev.preceding_message_id = Some(last.clone());
                }
                self.events.push((line_start_offset, ev));
            }
        }
        if let Some(ev) = build_claude_system_tool_result_event(
            obj,
            &mut self.tool_result_counters,
            self.next_event_index,
        ) {
            self.pending_tool_result_events
                .push((line_start_offset, ev));
            self.next_event_index += 1;
        }
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
            evidence: new_evidence(file_session_id),
        });
    }

    let mut state = ClaudeParseState::new(
        path,
        start_offset,
        file_session_id,
        options.last_user_text.as_deref(),
    )?;

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
            "assistant" => state.handle_assistant(&parsed, &obj, line_start_offset),
            "user" => state.handle_user(&parsed, &obj, line_start_offset, counter, capture_content),
            "system" => state.handle_system(&obj, line_start_offset),
            _ => {}
        }
    }

    // Move the accumulated working state out of `state` so the post-loop
    // assembly below reads the same locals the prior inline body did.
    let ClaudeParseState {
        file_session_id: _,
        evidence,
        nodes_by_uuid,
        mut invocation_cache,
        tool_result_counters: _,
        next_event_index: _,
        last_assistant_message_id: _,
        resume_marker_offset,
        current_user_text,
        working,
        order,
        message_id_first_offset,
        user_text_by_message_id,
        user_text_by_uuid,
        errored_tool_use_ids,
        replacement_meta_by_tool_use_id,
        user_rows_for_triad,
        events,
        pending_user_content,
        pending_tool_result_events,
        pending_relationships,
        pending_user_turns,
        seen_root_session_ids: _,
        seen_explicit_relationship_ids: _,
        pending_user_turn_inc_idx: _,
    } = state;

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
