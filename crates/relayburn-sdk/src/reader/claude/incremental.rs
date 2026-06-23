//! Claude incremental-parse engine.
//!
//! Mechanically split out of `reader/claude.rs` to shrink the oversized root
//! file. Holds the resume prescan (`prescan_nodes`), the per-line streaming
//! state machine (`ClaudeParseState`), and the driver `run_incremental` that
//! the public `parse_claude_session*` entry points wrap. Behavior is
//! unchanged from the inline implementation; only module placement,
//! visibility, and imports differ.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::reader::classifier::{detect_slash_triads, is_task_notification};
use crate::reader::git::resolve_project;
use crate::reader::inference::{RequestIdLookup, TurnKey};
use crate::reader::types::{
    CompactionEvent, ContentRecord, ContentStoreMode, RelationshipSourceKind, RelationshipType,
    SessionRelationshipRecord, SourceKind, StopReason, ToolResultEventRecord, TurnRecord,
    UserTurnRecord,
};
use crate::reader::user_turn::TokenCounter;

use super::relationships::{
    annotate_compaction_events, annotate_relationships_with_evidence, annotate_spawn_events,
    collect_explicit_claude_relationships_incremental, collect_subagent_relationships,
    derive_file_session_id_from_parts, emit_local_continuation_from_resume, new_evidence,
    record_evidence_from_line, record_explicit_relationship_evidence, record_resume_marker,
    ClaudeRelationshipEvidence, RelationshipKey,
};
use super::tool_results::{
    build_claude_system_tool_result_event, collect_replacement_meta, collect_tool_result_events,
    ReplacementMeta,
};
use super::{
    apply_classification, build_claude_fidelity, build_user_turn_record,
    collect_errored_tool_use_ids, extract_assistant_content, extract_files_touched,
    extract_plain_user_text_from_obj, extract_tool_calls, extract_user_content,
    ingest_assistant_record, register_assistant_node, register_user_node, resolve_subagent,
    string_field, InvocationInfo, LineNode, ParseIncrementalOptions, ParseIncrementalResult,
    WorkingRecord, SESSION_ID_KEYS, TIMESTAMP_KEYS,
};

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

pub(super) fn run_incremental<C: TokenCounter + ?Sized>(
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
