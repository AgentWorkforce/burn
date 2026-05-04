//! Claude Code session parser — Rust port of `packages/reader/src/claude.ts`.
//!
//! Covers `parse_claude_session`, `parse_claude_session_incremental`, and
//! `reconcile_claude_session_relationships`.
//!
//! The on-disk JSONL has a very loose shape (any extra fields permitted, any
//! field can be absent), so we keep raw lines as `serde_json::Value` and use
//! small accessor helpers rather than ahead-of-time deserialization. This
//! mirrors the TS implementation, which also walks records as `unknown`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::classifier::{classify_activity, ClassificationInput};
use crate::git::resolve_project;
use crate::hash::{args_hash, content_hash};
use crate::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode, ContentToolResult,
    ContentToolUse, Coverage, Fidelity, RelationshipSourceKind, RelationshipType,
    SessionRelationshipRecord, SourceKind, Subagent, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, Usage, UsageGranularity, UserTurnBlock,
    UserTurnRecord,
};
use crate::user_turn::{HeuristicCounter, TokenCounter, UserTurnTokenizer};

// ---------------------------------------------------------------------------
// Public surface.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
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
pub fn parse_claude_session_with_counter<P: AsRef<Path>, C: TokenCounter + ?Sized>(
    path: P,
    options: &ParseOptions,
    counter: &C,
) -> std::io::Result<ParseResult> {
    let path = path.as_ref();
    let content_mode = options.content_mode.unwrap_or(ContentStoreMode::Off);
    let capture_content = matches!(content_mode, ContentStoreMode::Full);

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut state = ParseState::new(options, path);

    for line in reader.lines() {
        let line = line?;
        state.ingest_line(&line, counter, capture_content);
    }

    Ok(state.finish(options, capture_content))
}

#[derive(Debug, Clone, Default)]
pub struct ParseIncrementalOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
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
    run_incremental(path.as_ref(), options, counter)
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

struct ParseState {
    /// messageId -> working assistant record. Ordered iteration uses `order`.
    working: HashMap<String, WorkingRecord>,
    order: Vec<String>,
    nodes_by_uuid: HashMap<String, LineNode>,
    invocation_cache: HashMap<String, Option<InvocationInfo>>,
    user_pending: Vec<(usize, ContentRecord)>,
    first_seq: HashMap<String, usize>,
    user_text_by_message_id: HashMap<String, String>,
    errored_tool_use_ids: HashSet<String>,
    replacement_meta_by_tool_use_id: HashMap<String, ReplacementMeta>,
    events: Vec<CompactionEvent>,
    user_turns: Vec<UserTurnRecord>,
    pending_user_turn_index: Option<usize>,
    last_assistant_message_id: Option<String>,
    current_user_text: String,
    seq: usize,
    tool_result_events: Vec<ToolResultEventRecord>,
    tool_result_counters: HashMap<String, u64>,
    next_event_index: u64,
    relationships: Vec<SessionRelationshipRecord>,
    seen_root_session_ids: HashSet<String>,
    seen_explicit_relationship_ids: HashSet<String>,
    file_session_id: Option<String>,
    evidence: ClaudeRelationshipEvidence,
}

impl ParseState {
    fn new(options: &ParseOptions, path: &Path) -> Self {
        let file_session_id = derive_file_session_id(options, path);
        let evidence = new_evidence(file_session_id.clone());
        Self {
            working: HashMap::new(),
            order: Vec::new(),
            nodes_by_uuid: HashMap::new(),
            invocation_cache: HashMap::new(),
            user_pending: Vec::new(),
            first_seq: HashMap::new(),
            user_text_by_message_id: HashMap::new(),
            errored_tool_use_ids: HashSet::new(),
            replacement_meta_by_tool_use_id: HashMap::new(),
            events: Vec::new(),
            user_turns: Vec::new(),
            pending_user_turn_index: None,
            last_assistant_message_id: None,
            current_user_text: String::new(),
            seq: 0,
            tool_result_events: Vec::new(),
            tool_result_counters: HashMap::new(),
            next_event_index: 0,
            relationships: Vec::new(),
            seen_root_session_ids: HashSet::new(),
            seen_explicit_relationship_ids: HashSet::new(),
            file_session_id,
            evidence,
        }
    }

    fn ingest_line<C: TokenCounter + ?Sized>(
        &mut self,
        raw: &str,
        counter: &C,
        capture_content: bool,
    ) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return,
        };
        let obj = match parsed.as_object() {
            Some(o) => o.clone(),
            None => return,
        };
        let line_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match line_type {
            "assistant" => self.ingest_assistant(&parsed, &obj, capture_content),
            "user" => self.ingest_user(&parsed, &obj, counter, capture_content),
            "system" => self.ingest_system(&obj),
            _ => {}
        }
        self.seq += 1;
    }

    fn ingest_assistant(
        &mut self,
        parsed: &Value,
        obj: &serde_json::Map<String, Value>,
        capture_content: bool,
    ) {
        let mid = obj
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);

        if let Some(mid) = mid.as_ref() {
            if let Some(idx) = self.pending_user_turn_index {
                if !self.working.contains_key(mid) {
                    self.user_turns[idx].following_message_id = Some(mid.clone());
                    self.pending_user_turn_index = None;
                }
            }
            if capture_content {
                self.first_seq.entry(mid.clone()).or_insert(self.seq);
            }
            self.user_text_by_message_id
                .entry(mid.clone())
                .or_insert_with(|| self.current_user_text.clone());
            self.last_assistant_message_id = Some(mid.clone());
        }

        let session_id = string_field(obj, "sessionId");
        let timestamp = string_field(obj, "timestamp");

        if let Some(ref sid) = session_id {
            if !sid.is_empty() {
                record_root(
                    &mut self.relationships,
                    &mut self.seen_root_session_ids,
                    sid,
                    timestamp.as_deref(),
                    self.file_session_id.as_deref(),
                );
                collect_explicit_claude_relationships(
                    obj,
                    &mut self.evidence,
                    &mut self.relationships,
                    &mut self.seen_explicit_relationship_ids,
                    self.file_session_id.as_deref().unwrap_or(sid.as_str()),
                    timestamp.as_deref(),
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

    fn ingest_user<C: TokenCounter + ?Sized>(
        &mut self,
        parsed: &Value,
        obj: &serde_json::Map<String, Value>,
        counter: &C,
        capture_content: bool,
    ) {
        register_user_node(parsed, &mut self.nodes_by_uuid);
        if let Some(text) = extract_plain_user_text_from_obj(obj) {
            if !text.is_empty() {
                self.current_user_text = text;
            }
        }
        collect_errored_tool_use_ids(obj, &mut self.errored_tool_use_ids);
        collect_replacement_meta(obj, &mut self.replacement_meta_by_tool_use_id);
        let session_id = string_field(obj, "sessionId");
        let timestamp = string_field(obj, "timestamp");
        if let Some(ref sid) = session_id {
            if !sid.is_empty() {
                record_root(
                    &mut self.relationships,
                    &mut self.seen_root_session_ids,
                    sid,
                    timestamp.as_deref(),
                    self.file_session_id.as_deref(),
                );
                collect_explicit_claude_relationships(
                    obj,
                    &mut self.evidence,
                    &mut self.relationships,
                    &mut self.seen_explicit_relationship_ids,
                    self.file_session_id.as_deref().unwrap_or(sid.as_str()),
                    timestamp.as_deref(),
                );
            }
        }
        record_evidence_from_line(&mut self.evidence, parsed);
        record_resume_marker(&mut self.evidence, obj);
        self.next_event_index = collect_tool_result_events(
            obj,
            &mut self.tool_result_events,
            &mut self.tool_result_counters,
            self.next_event_index,
        );
        if let Some(record) =
            build_user_turn_record(obj, self.last_assistant_message_id.as_deref(), counter)
        {
            let idx = self.user_turns.len();
            self.user_turns.push(record);
            self.pending_user_turn_index = Some(idx);
        }
        if capture_content {
            for c in extract_user_content(obj) {
                self.user_pending.push((self.seq, c));
            }
        }
    }

    fn ingest_system(&mut self, obj: &serde_json::Map<String, Value>) {
        if obj.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            let session_id = string_field(obj, "sessionId").unwrap_or_default();
            let ts = string_field(obj, "timestamp").unwrap_or_default();
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
                self.events.push(ev);
            }
        }
        if let Some(ev) = build_claude_system_tool_result_event(
            obj,
            &mut self.tool_result_counters,
            self.next_event_index,
        ) {
            self.tool_result_events.push(ev);
            self.next_event_index += 1;
        }
    }

    fn finish(self, options: &ParseOptions, capture_content: bool) -> ParseResult {
        let ParseState {
            working,
            order,
            nodes_by_uuid,
            mut invocation_cache,
            user_pending,
            first_seq,
            user_text_by_message_id,
            errored_tool_use_ids,
            replacement_meta_by_tool_use_id,
            mut events,
            user_turns,
            mut tool_result_events,
            mut relationships,
            evidence,
            ..
        } = self;

        let mut turns: Vec<TurnRecord> = Vec::new();
        let mut assistant_pending: Vec<(usize, usize, ContentRecord)> = Vec::new();
        for (i, id) in order.iter().enumerate() {
            let w = match working.get(id) {
                Some(w) => w,
                None => continue,
            };
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
                stop_reason: w.stop_reason.clone(),
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

            apply_classification(&mut record, w, &user_text_by_message_id, &errored_tool_use_ids);
            turns.push(record);

            if capture_content {
                let seq_for_msg = *first_seq.get(&w.message_id).unwrap_or(&0);
                for (sub, r) in extract_assistant_content(w).into_iter().enumerate() {
                    assistant_pending.push((seq_for_msg, sub + 1, r));
                }
            }
        }

        annotate_compaction_events(&mut events, &turns);
        collect_subagent_relationships(&turns, &mut relationships);
        annotate_spawn_events(&mut tool_result_events, &turns);
        emit_local_continuation_from_resume(&mut relationships, &evidence);
        annotate_relationships_with_evidence(&mut relationships, &evidence);

        let content: Vec<ContentRecord> = if capture_content {
            merge_content_by_order(user_pending, assistant_pending)
        } else {
            Vec::new()
        };

        ParseResult {
            turns,
            content,
            events,
            relationships,
            tool_result_events,
            user_turns,
            evidence,
        }
    }
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
    let session_id = string_field(obj, "sessionId").unwrap_or_default();
    let timestamp = string_field(obj, "timestamp").unwrap_or_default();
    let cwd = string_field(obj, "cwd");
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
    let uuid = string_field(obj, "uuid");
    let parent_uuid = string_field(obj, "parentUuid");

    let usage_with_cov = to_usage(msg.get("usage"));

    if let Some(w) = working.get_mut(&message_id) {
        if is_sidechain {
            w.is_sidechain = true;
        }
        if w.model.is_empty() && !model.is_empty() {
            w.model = model.clone();
        }
        if msg.contains_key("usage") {
            w.usage_coverage = merge_usage_coverage(&w.usage_coverage, &usage_with_cov.coverage);
        }
        if let Some(s) = stop_reason {
            w.stop_reason = Some(s);
        }
        for b in &blocks {
            w.blocks.push(b.clone());
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

fn register_user_node(line: &Value, nodes: &mut HashMap<String, LineNode>) {
    let mut node = match make_line_node(line, LineKind::User) {
        Some(n) => n,
        None => return,
    };
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
    loop {
        let node = match nodes.get(&current_uuid) {
            Some(n) => n.clone(),
            None => break,
        };
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
        let parent_agent = parent.agent_tool_use.clone();
        if node.kind == LineKind::User
            && parent.kind == LineKind::Assistant
            && parent_agent.is_some()
            && !node
                .tool_result_ids
                .as_ref()
                .is_some_and(|ids| ids.contains(&parent_agent.as_ref().unwrap().id))
        {
            let pat = parent_agent.unwrap();
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
    let session_id = string_field(line, "sessionId").unwrap_or_default();
    let message_id = string_field(line, "uuid").unwrap_or_default();
    let ts = string_field(line, "timestamp").unwrap_or_default();
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
                let mut rec = ContentRecord {
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
                        is_error: None,
                    }),
                };
                if bo.get("is_error").and_then(Value::as_bool) == Some(true) {
                    rec.tool_result.as_mut().unwrap().is_error = Some(true);
                }
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

fn merge_content_by_order(
    user_pending: Vec<(usize, ContentRecord)>,
    assistant_pending: Vec<(usize, usize, ContentRecord)>,
) -> Vec<ContentRecord> {
    let mut merged: Vec<(usize, usize, ContentRecord)> = Vec::new();
    for (seq, r) in user_pending {
        merged.push((seq, 0, r));
    }
    for (seq, sub, r) in assistant_pending {
        merged.push((seq, sub, r));
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    merged.into_iter().map(|(_, _, r)| r).collect()
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
    let session_id = first_nonempty_string(line, "sessionId")?;
    let user_uuid = first_nonempty_string(line, "uuid")?;
    let blocks = extract_user_turn_blocks(line, counter);
    if blocks.is_empty() {
        return None;
    }
    Some(UserTurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id,
        user_uuid,
        ts: string_field(line, "timestamp").unwrap_or_default(),
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
    let session_id = match string_field(line, "sessionId") {
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
    let message_id = string_field(line, "uuid");
    let ts = string_field(line, "timestamp");
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
        let call_index = *counters.get(&tu).unwrap_or(&0);
        counters.insert(tu.clone(), call_index + 1);
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
        };
    }
    if content.is_null() {
        return Measured::default();
    }
    match serde_json::to_string(content) {
        Ok(s) => Measured {
            length: Some(s.chars().count() as u64),
            hash: Some(content_hash(&s)),
        },
        Err(_) => Measured::default(),
    }
}

fn build_claude_system_tool_result_event(
    line: &serde_json::Map<String, Value>,
    counters: &mut HashMap<String, u64>,
    event_index: u64,
) -> Option<ToolResultEventRecord> {
    let session_id = first_string_field(line, &["sessionId", "session_id"])?;
    let tool_use_id = first_string_field(
        line,
        &[
            "parent_tool_use_id",
            "parentToolUseId",
            "parentToolUseID",
            "tool_use_id",
            "toolUseId",
        ],
    )?;
    let agent_id = first_string_field(line, &["agent_id", "agentId"]);
    let subagent_session_id =
        first_string_field(line, &["subagent_session_id", "subagentSessionId"]);
    if agent_id.is_none() && subagent_session_id.is_none() {
        return None;
    }
    let call_index = *counters.get(&tool_use_id).unwrap_or(&0);
    counters.insert(tool_use_id.clone(), call_index + 1);
    let status = claude_system_event_status(line);
    let mut record = ToolResultEventRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id,
        message_id: None,
        tool_use_id,
        call_index: Some(call_index),
        event_index,
        ts: first_string_field(line, &["timestamp", "ts"]),
        status,
        event_source: ToolResultEventSource::SubagentNotification,
        content_length: None,
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
    }
    Some(record)
}

fn claude_system_event_status(line: &serde_json::Map<String, Value>) -> ToolResultStatus {
    if line.get("is_error").and_then(Value::as_bool) == Some(true)
        || line.get("isError").and_then(Value::as_bool) == Some(true)
    {
        return ToolResultStatus::Errored;
    }
    let raw = first_string_field(
        line,
        &["status", "state", "result", "terminal_status", "terminalStatus"],
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

fn collect_errored_tool_use_ids(
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

fn apply_classification(
    record: &mut TurnRecord,
    w: &WorkingRecord,
    user_text_by_message_id: &HashMap<String, String>,
    errored: &HashSet<String>,
) {
    let user_text = user_text_by_message_id
        .get(&w.message_id)
        .cloned()
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
    record.activity = Some(result.activity);
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

fn record_root(
    out: &mut Vec<SessionRelationshipRecord>,
    seen: &mut HashSet<String>,
    session_id: &str,
    ts: Option<&str>,
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
    out.push(row);
}

fn collect_explicit_claude_relationships(
    line: &serde_json::Map<String, Value>,
    evidence: &mut ClaudeRelationshipEvidence,
    out: &mut Vec<SessionRelationshipRecord>,
    seen: &mut HashSet<String>,
    session_id: &str,
    fallback_ts: Option<&str>,
) {
    record_explicit_relationship_evidence(evidence, line);
    for row in build_explicit_claude_relationships(line, session_id, fallback_ts) {
        let key = relationship_key(&row);
        if !seen.insert(key) {
            continue;
        }
        out.push(row);
    }
}

fn build_explicit_claude_relationships(
    line: &serde_json::Map<String, Value>,
    session_id: &str,
    fallback_ts: Option<&str>,
) -> Vec<SessionRelationshipRecord> {
    let mut rows = Vec::new();
    let fork = first_string_field(line, &["forkSessionId", "fork_session_id"]);
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
    let cont = first_string_field(line, &["continuedFromSessionId", "continued_from_session_id"]);
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
    let ts = first_string_field(line, &["timestamp", "ts"])
        .or_else(|| fallback_ts.map(str::to_string));
    if let Some(t) = ts {
        row.ts = Some(t);
    }
    if let Some(s) = first_string_field(line, &["sourceSessionId", "source_session_id"]) {
        row.source_session_id = Some(s);
    }
    if let Some(s) = first_string_field(line, &["version", "sourceVersion", "source_version"]) {
        row.source_version = Some(s);
    }
    row
}

fn record_explicit_relationship_evidence(
    evidence: &mut ClaudeRelationshipEvidence,
    line: &serde_json::Map<String, Value>,
) {
    if let Some(c) =
        first_string_field(line, &["continuedFromSessionId", "continued_from_session_id"])
    {
        evidence.explicit_continuation_target_session_ids = Some(append_unique(
            evidence.explicit_continuation_target_session_ids.clone(),
            c,
        ));
    }
    if let Some(f) = first_string_field(line, &["forkSessionId", "fork_session_id"]) {
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

fn relationship_key(row: &SessionRelationshipRecord) -> String {
    let source = match row.source {
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
    let rt = match row.relationship_type {
        RelationshipType::Root => "root",
        RelationshipType::Continuation => "continuation",
        RelationshipType::Fork => "fork",
        RelationshipType::Subagent => "subagent",
    };
    format!(
        "{}|{}|{}|{}|{}|{}",
        source,
        row.session_id,
        rt,
        row.related_session_id.as_deref().unwrap_or(""),
        row.agent_id.as_deref().unwrap_or(""),
        row.parent_tool_use_id.as_deref().unwrap_or(""),
    )
}

fn has_relationship(rows: &[SessionRelationshipRecord], row: &SessionRelationshipRecord) -> bool {
    let key = relationship_key(row);
    rows.iter().any(|r| relationship_key(r) == key)
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
    if let Some(sid) = lo.get("sessionId").and_then(Value::as_str) {
        if !sid.is_empty() {
            if !evidence.in_log_session_ids.iter().any(|s| s == sid) {
                evidence.in_log_session_ids.push(sid.to_string());
            }
            if evidence.first_ts.is_none() {
                if let Some(ts) = lo.get("timestamp").and_then(Value::as_str) {
                    if !ts.is_empty() {
                        evidence.first_ts = Some(ts.to_string());
                    }
                }
            }
        }
    }
    if evidence.source_version.is_none() {
        if let Some(v) = lo.get("version").and_then(Value::as_str) {
            if !v.is_empty() {
                evidence.source_version = Some(v.to_string());
            }
        }
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
        let token_end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
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
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let length = end_offset.min(size);
    if length == 0 {
        return Ok(PrescanOutput {
            last_assistant_message_id: None,
            next_event_index: 0,
        });
    }
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    let mut p: usize = 0;
    let mut last_assistant_message_id: Option<String> = None;
    let mut next_event_index: u64 = 0;
    while p < buf.len() {
        let nl_idx = match buf[p..].iter().position(|&b| b == b'\n') {
            Some(i) => p + i,
            None => break,
        };
        let raw = std::str::from_utf8(&buf[p..nl_idx]).unwrap_or("").trim();
        p = nl_idx + 1;
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
                register_user_node(&parsed, nodes_by_uuid);
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
            "system" => {
                if build_claude_system_tool_result_event(
                    &obj,
                    tool_result_counters,
                    next_event_index,
                )
                .is_some()
                {
                    next_event_index += 1;
                }
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
    seen: &mut HashSet<String>,
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
    let mut user_text_by_message_id: HashMap<String, String> = HashMap::new();
    let mut errored_tool_use_ids: HashSet<String> = HashSet::new();
    let mut replacement_meta_by_tool_use_id: HashMap<String, ReplacementMeta> = HashMap::new();
    let mut events: Vec<(u64, CompactionEvent)> = Vec::new();
    let mut pending_user_content: Vec<(u64, ContentRecord)> = Vec::new();
    let mut pending_tool_result_events: Vec<(u64, ToolResultEventRecord)> = Vec::new();
    let mut pending_relationships: Vec<(u64, SessionRelationshipRecord)> = Vec::new();
    let mut pending_user_turns: Vec<(u64, UserTurnRecord)> = Vec::new();
    let mut seen_root_session_ids: HashSet<String> = HashSet::new();
    let mut seen_explicit_relationship_ids: HashSet<String> = HashSet::new();
    let mut pending_user_turn_inc_idx: Option<usize> = None;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf: Vec<u8> = Vec::with_capacity((size - start_offset) as usize);
    file.read_to_end(&mut buf)?;

    let mut p: usize = 0;
    let mut cursor_offset: u64 = start_offset; // position past last complete \n
    while p < buf.len() {
        let nl_idx = match buf[p..].iter().position(|&b| b == b'\n') {
            Some(i) => p + i,
            None => break,
        };
        let line_start_offset = start_offset + p as u64;
        let line_end_offset = start_offset + nl_idx as u64 + 1;
        let trimmed = std::str::from_utf8(&buf[p..nl_idx]).unwrap_or("").trim();
        p = nl_idx + 1;
        cursor_offset = line_end_offset;
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
                            pending_user_turns[idx].1.following_message_id =
                                Some(mid_str.clone());
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
                let session_id = string_field(&obj, "sessionId");
                let timestamp = string_field(&obj, "timestamp");
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
                register_user_node(&parsed, &mut nodes_by_uuid);
                if let Some(text) = extract_plain_user_text_from_obj(&obj) {
                    if !text.is_empty() {
                        current_user_text = text;
                    }
                }
                collect_errored_tool_use_ids(&obj, &mut errored_tool_use_ids);
                collect_replacement_meta(&obj, &mut replacement_meta_by_tool_use_id);
                let session_id = string_field(&obj, "sessionId");
                let timestamp = string_field(&obj, "timestamp");
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
                if let Some(record) = build_user_turn_record(
                    &obj,
                    last_assistant_message_id.as_deref(),
                    counter,
                ) {
                    let idx = pending_user_turns.len();
                    pending_user_turns.push((line_start_offset, record));
                    pending_user_turn_inc_idx = Some(idx);
                }
                if capture_content {
                    for c in extract_user_content(&obj) {
                        pending_user_content.push((line_start_offset, c));
                    }
                }
            }
            "system" => {
                if obj.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                    let session_id = string_field(&obj, "sessionId").unwrap_or_default();
                    let ts = string_field(&obj, "timestamp").unwrap_or_default();
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
    // messages are complete.
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
    let end_offset = earliest_incomplete.unwrap_or(cursor_offset);

    // Emit completed turns. In-progress messages (no stop_reason) are deferred
    // — `end_offset` already backs up to before their first byte so the next
    // call re-reads them.
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut assistant_pending: Vec<(u64, usize, ContentRecord)> = Vec::new();
    for (i, id) in order.iter().enumerate() {
        let w = match working.get(id) {
            Some(w) => w,
            None => continue,
        };
        if w.stop_reason.is_none() {
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
            stop_reason: w.stop_reason.clone(),
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
        apply_classification(&mut record, w, &user_text_by_message_id, &errored_tool_use_ids);
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

    Ok(ParseIncrementalResult {
        turns,
        content,
        events: emitted_events,
        relationships: emitted_relationships,
        tool_result_events: emitted_tool_result_events,
        user_turns: emitted_user_turns,
        end_offset,
        last_user_text: current_user_text,
        evidence,
    })
}

// ---------------------------------------------------------------------------
// Misc helpers.
// ---------------------------------------------------------------------------

fn derive_file_session_id(options: &ParseOptions, _path: &Path) -> Option<String> {
    derive_file_session_id_from_parts(
        options.file_session_id.as_deref(),
        options.session_path.as_deref(),
    )
}

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

fn string_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn first_nonempty_string(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn first_string_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(Value::as_str) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
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
    fn simple_turn_parses() {
        let path = fixture("simple-turn.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1, "expected one turn");
        let t = &res.turns[0];
        assert_eq!(t.v, 1);
        assert_eq!(t.source, SourceKind::ClaudeCode);
        assert_eq!(t.message_id, "msg_simple_1");
        assert_eq!(t.model, "claude-sonnet-4-6");
        assert_eq!(t.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(t.usage.input, 10);
        assert_eq!(t.usage.output, 5);
        assert_eq!(t.usage.cache_read, 500);
        assert_eq!(t.usage.cache_create_5m, 80);
        assert_eq!(t.usage.cache_create_1h, 20);
        assert_eq!(t.tool_calls.len(), 0);
        assert!(t.files_touched.is_none());
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
        assert_eq!(t.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(t.ts, "2026-04-20T00:00:01.000Z");
    }

    #[test]
    fn files_touched_excludes_grep_and_bash() {
        let path = fixture("files-touched.jsonl");
        let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
        assert_eq!(res.turns.len(), 1);
        let t = &res.turns[0];
        assert_eq!(t.tool_calls.len(), 3);
        assert_eq!(
            t.files_touched.as_ref().map(|v| v.as_slice()),
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

    use crate::types::ActivityCategory;
    use std::io::Write as _;

    fn read_bytes(p: &std::path::Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    fn write_bytes(p: &std::path::Path, b: &[u8]) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(b).unwrap();
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
        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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
        assert!(asst_first
            .iter()
            .all(|c| c.message_id == "msg_done_1"));

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
        assert!(asst_second
            .iter()
            .all(|c| c.message_id == "msg_inprog_1"));
        assert!(asst_second.iter().any(|c| matches!(c.kind, ContentKind::Text)
            && c.text.as_deref() == Some("done now")));
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
        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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
        assert_eq!(second.turns[0].stop_reason.as_deref(), Some("end_turn"));
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

        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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
        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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

        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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
        let first =
            parse_claude_session_incremental(&working, &ParseIncrementalOptions::default())
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
}
