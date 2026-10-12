//! Walks over Claude's `parentUuid` graph: the prompt that rooted an
//! assistant turn, and the Task/Agent invocation a sidechain turn runs
//! under.

use std::collections::HashSet;

use ai_hist::{BlockKind, Message, Role};

use super::records::Replacements;
use super::{is_user_record, prompt_text, Transcript};
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::types::{ActivityCategory, Subagent, TurnRecord};

/// The Task/Agent call a sidechain hangs off.
struct Invocation {
    /// The sidechain's first record: the prompt the parent handed it.
    root: String,
    parent_tool_use_id: Option<String>,
    subagent_type: Option<String>,
    description: Option<String>,
    parent_agent_id: Option<String>,
}

impl Transcript<'_> {
    pub(super) fn refine_turn(&self, turn: &mut TurnRecord, replacements: &Replacements<'_>) {
        for call in &mut turn.tool_calls {
            if let Some(meta) = replacements.for_call(&call.id) {
                call.replaced_tools = meta.replaced_tools.clone();
                call.collapsed_calls = meta.collapsed_calls;
            }
        }
        turn.subagent = self.subagent(turn);
        self.classify(turn);
    }

    fn subagent(&self, turn: &TurnRecord) -> Option<Subagent> {
        let messages = self.messages_of(turn);
        if !messages.iter().any(|m| m.is_sidechain == Some(true)) {
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
        let start = messages.first().and_then(|m| m.message_id.as_deref());
        let Some(info) = start.and_then(|s| self.invocation(s)) else {
            return Some(sub);
        };
        sub.agent_id = Some(info.root);
        sub.parent_tool_use_id = info.parent_tool_use_id;
        sub.subagent_type = info.subagent_type;
        sub.description = info.description;
        sub.parent_agent_id = info
            .parent_agent_id
            .or_else(|| Some(self.session_id().to_string()));
        Some(sub)
    }

    /// The invocation the record `start` runs under.
    fn invocation(&self, start: &str) -> Option<Invocation> {
        let (node, parent, call) = self.spawn_of(start)?;
        let arg = |k: &str| {
            call.args
                .as_ref()
                .and_then(|a| a.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        // A spawn from inside a sidechain hangs under that sidechain's own
        // invocation.
        let parent_agent_id = parent
            .message_id
            .as_deref()
            .filter(|_| parent.is_sidechain == Some(true))
            .and_then(|id| self.spawn_of(id))
            .and_then(|(root, _, _)| root.message_id.clone());
        Some(Invocation {
            root: node.message_id.clone().unwrap_or_default(),
            parent_tool_use_id: Some(call.tool_use_id.clone()).filter(|id| !id.is_empty()),
            subagent_type: arg("subagent_type"),
            description: arg("description"),
            parent_agent_id,
        })
    }

    /// Walk up from `start` to the first user record whose parent is an
    /// assistant that spawned a Task/Agent call the record does not answer:
    /// that record, its parent, and the call.
    fn spawn_of(&self, start: &str) -> Option<(&Message, &Message, &ai_hist::ToolCall)> {
        let mut seen = HashSet::new();
        let mut node = *self.by_id.get(start)?;
        loop {
            if !seen.insert(node.message_id.as_deref()?) {
                return None;
            }
            let parent = *self.by_id.get(node.parent_id.as_deref()?)?;
            if let Some(call) = self.spawn_call(node, parent) {
                return Some((node, parent, call));
            }
            node = parent;
        }
    }

    /// The spawn call `node` was handed as a prompt: the parent assistant's
    /// first Task/Agent call, unless `node` carries tool results (that
    /// call's, or a sibling call's) rather than a prompt.
    fn spawn_call(&self, node: &Message, parent: &Message) -> Option<&ai_hist::ToolCall> {
        if !is_user_record(node) || parent.role != Role::Assistant {
            return None;
        }
        let parent_id = parent.message_id.as_deref()?;
        let call = self.ev.tool_calls.iter().find(|c| {
            c.message_id.as_deref() == Some(parent_id)
                && matches!(c.name.as_str(), "Agent" | "Task")
        })?;
        // A record carrying tool results answers the parent's calls; only a
        // prompt starts a sidechain.
        let answers = node.blocks.iter().any(|b| b.kind == BlockKind::ToolResult);
        (!answers).then_some(call)
    }

    /// Activity, retries, and edit flag from the turn's text plus the prompt
    /// that rooted it; a turn rooted in a slash-command triad is `Skill`.
    fn classify(&self, turn: &mut TurnRecord) {
        let messages = self.messages_of(turn);
        let Some(first) = messages.first() else {
            return;
        };
        let root = first
            .message_id
            .as_deref()
            .and_then(|id| self.prompt_root(id));
        let prompt = root
            .and_then(|r| self.by_id.get(r))
            .and_then(|m| prompt_text(m))
            .or_else(|| self.preceding_prompt(first));
        let mut parts: Vec<String> = prompt.into_iter().collect();
        let assistant = assistant_text(messages);
        if !assistant.is_empty() {
            parts.push(assistant);
        }
        let result = classify_activity(ClassificationInput {
            tool_calls: &turn.tool_calls,
            text: &parts.join("\n"),
            has_failed_tool: turn.tool_calls.iter().any(|c| c.is_error == Some(true)),
            reasoning_tokens: turn.usage.reasoning,
        });
        turn.activity = Some(match root {
            Some(r) if self.skill_messages.contains(r) => ActivityCategory::Skill,
            _ => result.activity,
        });
        turn.retries = Some(result.retries);
        turn.has_edits = Some(result.has_edits);
    }

    /// The nearest prompt record up the `parentUuid` chain from `start`.
    fn prompt_root(&self, start: &str) -> Option<&str> {
        let mut seen = HashSet::new();
        let mut node = *self.by_id.get(start)?;
        loop {
            let id = node.message_id.as_deref()?;
            if !seen.insert(id) {
                return None;
            }
            if prompt_text(node).is_some() {
                return Some(id);
            }
            node = *self.by_id.get(node.parent_id.as_deref()?)?;
        }
    }

    /// The last prompt before `message` in transcript order, for records
    /// whose chain does not reach one.
    fn preceding_prompt(&self, message: &Message) -> Option<String> {
        let at = self.position.get(message.message_id.as_deref()?).copied()?;
        self.ev.messages[..at].iter().rev().find_map(prompt_text)
    }
}

fn assistant_text(messages: &[&Message]) -> String {
    let parts: Vec<&str> = messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter(|b| b.kind == BlockKind::Text)
        .filter_map(|b| b.text.as_deref())
        .filter(|t| !t.is_empty())
        .collect();
    parts.join("\n")
}
