//! Claude tool-result event extraction.
//!
//! Builds `tool_result_event` rows from `tool_result` content blocks and from
//! Claude system subagent-notification lines, measures payload size / hashes /
//! truncation, and recovers tool-replacement metadata (`_meta.replaces`,
//! `collapsedCalls`). Split out of `claude.rs`; the parse engine there calls
//! these helpers per line while walking a session.

use std::collections::HashMap;

use serde_json::Value;

use super::{first_present, string_field, SESSION_ID_KEYS, TIMESTAMP_KEYS};
use crate::reader::hash::content_hash;
use crate::reader::types::{
    SourceKind, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus,
};

#[derive(Debug, Clone, Default)]
pub(super) struct ReplacementMeta {
    pub(super) replaced_tools: Option<Vec<String>>,
    pub(super) collapsed_calls: Option<u64>,
}

pub(super) fn collect_tool_result_events(
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
pub(super) struct Measured {
    pub(super) length: Option<u64>,
    pub(super) hash: Option<String>,
    /// Raw UTF-8 byte length of the materialized text (the same string the
    /// hash is computed against). Added in #436 so hotspots can rank tools
    /// by raw payload bytes alongside post-truncation tokens.
    pub(super) byte_length: Option<u64>,
    /// `Some(true)` when a recognized truncation marker was detected in
    /// the payload (see `detect_truncation_marker`). `None` when no
    /// payload was available; `Some(false)` when payload was inspected
    /// and looked complete.
    pub(super) truncated: Option<bool>,
}

pub(super) fn measure_tool_result(content: &Value) -> Measured {
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
pub(super) fn detect_truncation_marker(s: &str) -> bool {
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

pub(super) fn build_claude_system_tool_result_event(
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

pub(super) fn collect_replacement_meta(
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
