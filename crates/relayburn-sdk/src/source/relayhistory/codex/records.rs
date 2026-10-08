//! Record constructors for the Codex task state machine.

use serde_json::Value;

use crate::reader::types::{
    ContentKind, ContentRecord, ContentRole, ContentToolResult, ContentToolUse,
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, SourceKind,
    ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, UserTurnBlock, UserTurnRecord,
};
use crate::util::time::format_iso_ms;

/// A content row; `message_id` is the task it belongs to, empty until the
/// task opens.
pub(super) fn content(
    session_id: &str,
    message_id: &str,
    ts_ms: i64,
    role: ContentRole,
    kind: ContentKind,
) -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        ts: format_iso_ms(ts_ms),
        role,
        kind,
        text: None,
        tool_use: None,
        tool_result: None,
    }
}

pub(super) fn text_content(
    session_id: &str,
    message_id: &str,
    ts_ms: i64,
    role: ContentRole,
    kind: ContentKind,
    text: &str,
) -> ContentRecord {
    ContentRecord {
        text: Some(text.to_string()),
        ..content(session_id, message_id, ts_ms, role, kind)
    }
}

pub(super) fn tool_use_content(
    session_id: &str,
    message_id: &str,
    ts_ms: i64,
    tool_use: ContentToolUse,
) -> ContentRecord {
    ContentRecord {
        tool_use: Some(tool_use),
        ..content(
            session_id,
            message_id,
            ts_ms,
            ContentRole::Assistant,
            ContentKind::ToolUse,
        )
    }
}

pub(super) fn tool_result_content(
    session_id: &str,
    message_id: &str,
    ts_ms: i64,
    tool_use_id: &str,
    output: &str,
) -> ContentRecord {
    ContentRecord {
        tool_result: Some(ContentToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: Value::String(output.to_string()),
            is_error: None,
        }),
        ..content(
            session_id,
            message_id,
            ts_ms,
            ContentRole::ToolResult,
            ContentKind::ToolResult,
        )
    }
}

/// One tool-result event before its task settles its status.
pub(super) struct ResultEvent<'a> {
    pub session_id: &'a str,
    pub message_id: Option<String>,
    pub tool_use_id: &'a str,
    pub call_index: u64,
    pub event_index: u64,
    pub ts_ms: i64,
    pub status: ToolResultStatus,
    pub event_source: ToolResultEventSource,
}

impl ResultEvent<'_> {
    pub(super) fn record(self) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: self.session_id.to_string(),
            message_id: self.message_id,
            tool_use_id: self.tool_use_id.to_string(),
            call_index: Some(self.call_index),
            event_index: self.event_index,
            ts: Some(format_iso_ms(self.ts_ms)),
            status: self.status,
            event_source: self.event_source,
            content_length: None,
            output_bytes: None,
            output_truncated: None,
            content_hash: None,
            is_error: (self.status == ToolResultStatus::Errored).then_some(true),
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }
}

/// The status a subagent's terminal notification reports.
pub(super) fn notification_status(payload: Option<&Value>) -> ToolResultStatus {
    if let Some(success) = payload
        .and_then(|p| p.get("success"))
        .and_then(Value::as_bool)
    {
        return if success {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Errored
        };
    }
    let status = payload
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    match status.as_deref() {
        Some("errored" | "failed" | "error") => ToolResultStatus::Errored,
        Some("cancelled" | "canceled") => ToolResultStatus::Cancelled,
        _ => ToolResultStatus::Completed,
    }
}

/// The first non-empty string among `keys` of a JSON object.
pub(super) fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    keys.iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

pub(super) const AGENT_ID_KEYS: &[&str] = &["agent_id", "subagent_id", "session_id"];

/// The agent a `spawn_agent` output names; the output is JSON text.
pub(super) fn spawned_agent_id(output: &str) -> Option<String> {
    string_field(&serde_json::from_str(output).ok()?, AGENT_ID_KEYS)
}

/// The human (or tool-output) input gathered before a task starts.
#[derive(Default)]
pub(super) struct Slot {
    pub blocks: Vec<UserTurnBlock>,
    pub preceding_message_id: Option<String>,
    pub ts_ms: Option<i64>,
}

impl Slot {
    pub(super) fn push(&mut self, block: UserTurnBlock, ts_ms: i64) {
        self.blocks.push(block);
        self.ts_ms.get_or_insert(ts_ms);
    }

    pub(super) fn record(self, session_id: &str, turn_id: &str, task_ts_ms: i64) -> UserTurnRecord {
        let preceding = self.preceding_message_id.as_deref().unwrap_or("start");
        UserTurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: session_id.to_string(),
            user_uuid: format!("{session_id}:{preceding}->{turn_id}"),
            ts: format_iso_ms(self.ts_ms.unwrap_or(task_ts_ms)),
            preceding_message_id: self.preceding_message_id,
            following_message_id: Some(turn_id.to_string()),
            blocks: self.blocks,
        }
    }
}

/// A `spawn_agent` call awaiting the agent id it started.
pub(super) struct Spawn {
    pub subagent_type: Option<String>,
    pub description: Option<String>,
    pub agent_id: Option<String>,
    pub emitted: bool,
}

impl Spawn {
    pub(super) fn from_args(args: Option<&Value>) -> Self {
        let field = |keys: &[&str]| args.and_then(|a| string_field(a, keys));
        Self {
            subagent_type: field(&["subagent_type", "agent_type", "type"]),
            description: field(&["description", "task", "prompt"]),
            agent_id: field(AGENT_ID_KEYS),
            emitted: false,
        }
    }

    /// The subagent edge, once per spawn and only once its agent is known.
    pub(super) fn relationship(
        &mut self,
        session_id: &str,
        call_id: &str,
        ts_ms: i64,
    ) -> Option<SessionRelationshipRecord> {
        if self.emitted {
            return None;
        }
        let agent_id = self.agent_id.clone()?;
        self.emitted = true;
        Some(SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::Codex,
            session_id: agent_id.clone(),
            related_session_id: Some(session_id.to_string()),
            relationship_type: RelationshipType::Subagent,
            ts: Some(format_iso_ms(ts_ms)),
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: Some(call_id.to_string()),
            agent_id: Some(agent_id),
            subagent_type: self.subagent_type.clone(),
            description: self.description.clone(),
        })
    }
}
