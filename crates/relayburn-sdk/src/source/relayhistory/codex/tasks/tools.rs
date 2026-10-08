//! Tool calls, their outputs, and subagent lifecycle inside a Codex task.

use ai_hist::{Marker, ToolResult};
use serde_json::Value;

use super::super::events::ToolUse;
use super::super::records::{
    notification_status, spawned_agent_id, string_field, tool_result_content, tool_use_content,
    ResultEvent, Spawn, AGENT_ID_KEYS,
};
use super::super::targets::{custom_tool_target, function_call_target};
use super::Tasks;
use crate::reader::hash::args_hash;
use crate::reader::types::{
    ContentToolUse, ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus,
    UserTurnBlock, UserTurnBlockKind,
};
use crate::reader::user_turn::bytes_to_approx_tokens;

impl Tasks<'_> {
    /// A `function_call` / `custom_tool_call` inside a task, once per call id.
    pub(super) fn tool_use(&mut self, tool_use: &ToolUse<'_>) {
        let session_id = self.session_id;
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let call = tool_use.call;
        if !open.seen_calls.insert(call.tool_use_id.clone()) {
            return;
        }
        let args = call.args.as_ref().filter(|a| a.is_object());
        let input = args.and_then(Value::as_object).cloned().unwrap_or_default();
        let target = if tool_use.custom {
            let patch = input
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default();
            custom_tool_target(&call.name, patch)
        } else {
            function_call_target(&call.name, args)
        };
        open.tool_calls.push(ToolCall {
            id: call.tool_use_id.clone(),
            name: call.name.clone(),
            target,
            args_hash: args_hash(&Value::Object(input.clone())),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        });
        if call.name == "spawn_agent" && !tool_use.custom {
            let mut spawn = Spawn::from_args(args);
            open.relationships.extend(spawn.relationship(
                session_id,
                &call.tool_use_id,
                tool_use.ts_ms,
            ));
            open.spawns.insert(call.tool_use_id.clone(), spawn);
        }
        let content = tool_use_content(
            session_id,
            &open.turn_id,
            tool_use.ts_ms,
            ContentToolUse {
                id: call.tool_use_id.clone(),
                name: call.name.clone(),
                input: input.into_iter().collect(),
            },
        );
        open.content.push(content);
    }

    /// A `function_call_output` / `custom_tool_call_output`.
    pub(super) fn tool_output(&mut self, line: u64, result: &ToolResult) {
        let Some(call_id) = result.tool_use_id.as_deref() else {
            return;
        };
        let output = result.text.as_deref().unwrap_or_default();
        let bytes = result
            .payload_bytes
            .map_or(output.len() as u64, |b| b.max(0) as u64);
        let block = UserTurnBlock {
            kind: UserTurnBlockKind::ToolResult,
            tool_use_id: Some(call_id.to_string()),
            byte_len: bytes,
            approx_tokens: bytes_to_approx_tokens(bytes),
            is_error: None,
        };
        self.slot.push(block, result.ts_ms);
        let in_task_failure = self.open.is_some() && self.errored.contains(call_id);
        let status = if in_task_failure {
            ToolResultStatus::Errored
        } else {
            ToolResultStatus::Unknown
        };
        let mut event = self.result_event(
            call_id,
            result.ts_ms,
            status,
            ToolResultEventSource::FunctionCallOutput,
        );
        event.content_length = Some(bytes);
        event.output_bytes = Some(bytes);
        event.content_hash = result.payload_hash.clone();
        let session_id = self.session_id;
        match self.open.as_mut() {
            Some(open) => {
                if let Some(spawn) = open.spawns.get_mut(call_id) {
                    if let Some(agent) = spawned_agent_id(output) {
                        spawn.agent_id = Some(agent.clone());
                        event.agent_id = Some(agent.clone());
                        event.subagent_session_id = Some(agent);
                    }
                    open.relationships.extend(spawn.relationship(
                        session_id,
                        call_id,
                        result.ts_ms,
                    ));
                }
                open.events.push(event);
                let content =
                    tool_result_content(session_id, &open.turn_id, result.ts_ms, call_id, output);
                open.content.push(content);
            }
            None => {
                self.events.push((line, event));
                let content = tool_result_content(session_id, "", result.ts_ms, call_id, output);
                self.pending_content.push(content);
            }
        }
    }

    /// A subagent's terminal lifecycle notification: a result of the call
    /// that spawned it, and the agent id when the spawn did not name one.
    pub(super) fn subagent_done(&mut self, line: u64, marker: &Marker) {
        let payload = marker.payload.as_ref();
        let call_id = payload
            .and_then(|p| string_field(p, &["call_id"]))
            .or_else(|| marker.parent_id.clone().filter(|id| !id.is_empty()));
        let Some(call_id) = call_id else { return };
        let ts_ms = marker.ts_ms.unwrap_or_default();
        let status = notification_status(payload);
        let mut event = self.result_event(
            &call_id,
            ts_ms,
            status,
            ToolResultEventSource::SubagentNotification,
        );
        let agent = payload.and_then(|p| string_field(p, AGENT_ID_KEYS));
        event.agent_id.clone_from(&agent);
        event.subagent_session_id.clone_from(&agent);
        let session_id = self.session_id;
        match self.open.as_mut() {
            Some(open) => {
                if let (Some(spawn), Some(agent)) = (open.spawns.get_mut(&call_id), agent) {
                    if spawn.agent_id.is_none() {
                        spawn.agent_id = Some(agent);
                        open.relationships
                            .extend(spawn.relationship(session_id, &call_id, ts_ms));
                    }
                }
                open.events.push(event);
            }
            None => self.events.push((line, event)),
        }
    }

    /// The next result event for `call_id`, numbered per call and per session.
    fn result_event(
        &mut self,
        call_id: &str,
        ts_ms: i64,
        status: ToolResultStatus,
        event_source: ToolResultEventSource,
    ) -> ToolResultEventRecord {
        let counter = self.call_counters.entry(call_id.to_string()).or_insert(0);
        let call_index = *counter;
        *counter += 1;
        let event_index = self.next_event_index;
        self.next_event_index += 1;
        ResultEvent {
            session_id: self.session_id,
            message_id: self.open.as_ref().map(|o| o.turn_id.clone()),
            tool_use_id: call_id,
            call_index,
            event_index,
            ts_ms,
            status,
            event_source,
        }
        .record()
    }
}
