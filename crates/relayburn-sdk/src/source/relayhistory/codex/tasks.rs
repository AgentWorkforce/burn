//! Burn's Codex turn is the task: `task_started` opens it, the matching
//! `task_complete` commits it, and a `task_started` before that closes it
//! uncommitted. Everything derived inside a task waits for its commit;
//! nothing after the last committed task is emitted, because a live
//! rollout's open task is still being written.

use std::collections::{BTreeSet, HashMap, HashSet};

use ai_hist::SessionEvidence;

#[cfg(test)]
mod tests;
mod tools;

use super::events::{Event, Located};
use super::records::{text_content, Slot, Spawn};
use super::snapshots::{read_snapshot, spend, CounterError, Counters};
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::resolve_project;
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, Coverage, Fidelity,
    SessionRelationshipRecord, SourceKind, ToolCall, ToolResultEventRecord, ToolResultStatus,
    TurnRecord, Usage, UsageGranularity, UserTurnBlock, UserTurnBlockKind, UserTurnRecord,
};
use crate::reader::user_turn::{join_nonempty, HeuristicCounter};
use crate::util::time::format_iso_ms;

/// What the committed tasks of one session derive.
#[derive(Default)]
pub(super) struct Derived {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub user_turns: Vec<UserTurnRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub compactions: Vec<CompactionEvent>,
    pub subagents: Vec<SessionRelationshipRecord>,
    /// Whether any task committed.
    pub committed: bool,
}

/// The task being read.
struct Open {
    turn_id: String,
    ts_ms: i64,
    start: Counters,
    usage_observed: bool,
    counter_error: Option<CounterError>,
    tool_calls: Vec<ToolCall>,
    seen_calls: HashSet<String>,
    files: BTreeSet<String>,
    user_text: String,
    assistant_text: String,
    content: Vec<ContentRecord>,
    events: Vec<ToolResultEventRecord>,
    relationships: Vec<SessionRelationshipRecord>,
    spawns: HashMap<String, Spawn>,
}

pub(super) struct Tasks<'a> {
    ev: &'a SessionEvidence,
    session_id: &'a str,
    /// Calls Codex reported failed (`exec_command_end` exit code,
    /// `patch_apply_end` success).
    errored: HashSet<&'a str>,
    /// turn id → (model, cwd) from the turn's `turn_context` record, else
    /// as stamped on its messages.
    contexts: HashMap<&'a str, (Option<&'a str>, Option<&'a str>)>,
    cumulative: Counters,
    open: Option<Open>,
    pending_user_text: String,
    pending_content: Vec<ContentRecord>,
    finished: Vec<(TurnRecord, Vec<ContentRecord>)>,
    committed_turns: usize,
    slot: Slot,
    user_turns: Vec<UserTurnRecord>,
    committed_user_turns: usize,
    last_completed: Option<(String, u64)>,
    next_event_index: u64,
    call_counters: HashMap<String, u64>,
    /// Records outside a task, with the line that emitted them.
    events: Vec<(u64, ToolResultEventRecord)>,
    relationships: Vec<(u64, SessionRelationshipRecord)>,
    compactions: Vec<(u64, CompactionEvent)>,
    committed_line: Option<u64>,
}

impl<'a> Tasks<'a> {
    pub(super) fn new(ev: &'a SessionEvidence) -> Self {
        let mut contexts: HashMap<&str, (Option<&str>, Option<&str>)> = HashMap::new();
        for marker in ev.markers.iter().filter(|m| m.kind == "turn_context") {
            let (Some(turn), Some(payload)) = (marker.turn_id.as_deref(), marker.payload.as_ref())
            else {
                continue;
            };
            let field = |key: &str| payload.get(key).and_then(|v| v.as_str());
            let slot = contexts.entry(turn).or_default();
            slot.0 = slot.0.or(field("model"));
            slot.1 = slot.1.or(field("cwd"));
        }
        for m in &ev.messages {
            if let Some(turn) = m.turn_id.as_deref() {
                let slot = contexts.entry(turn).or_default();
                slot.0 = slot.0.or(m.model.as_deref());
                slot.1 = slot.1.or(m.cwd.as_deref());
            }
        }
        Self {
            ev,
            session_id: &ev.session.session_id,
            errored: ev
                .tool_calls
                .iter()
                .filter(|c| c.is_error == Some(true))
                .map(|c| c.tool_use_id.as_str())
                .collect(),
            contexts,
            cumulative: Counters::default(),
            open: None,
            pending_user_text: String::new(),
            pending_content: Vec::new(),
            finished: Vec::new(),
            committed_turns: 0,
            slot: Slot::default(),
            user_turns: Vec::new(),
            committed_user_turns: 0,
            last_completed: None,
            next_event_index: 0,
            call_counters: HashMap::new(),
            events: Vec::new(),
            relationships: Vec::new(),
            compactions: Vec::new(),
            committed_line: None,
        }
    }

    pub(super) fn run(mut self, stream: &[Located<'a>]) -> Derived {
        for located in stream {
            self.apply(located.line, &located.event);
        }
        self.into_derived()
    }

    fn apply(&mut self, line: u64, event: &Event<'a>) {
        match event {
            Event::TaskStarted { turn_id, ts_ms } => self.task_started(turn_id, *ts_ms),
            Event::TaskComplete { turn_id } => self.task_complete(line, turn_id),
            Event::Compacted { ts_ms } => self.compacted(line, *ts_ms),
            Event::UsageSnapshot { info } => self.usage_snapshot(info),
            Event::UserText { text, ts_ms } => self.user_text(text, *ts_ms),
            Event::AssistantText { text, ts_ms } => self.assistant_output(text, *ts_ms, false),
            Event::Reasoning { text, ts_ms } => self.assistant_output(text, *ts_ms, true),
            Event::ToolUse(tool_use) => self.tool_use(tool_use),
            Event::ToolOutput { result } => self.tool_output(line, result),
            Event::SubagentDone { marker } => self.subagent_done(line, marker),
            Event::FileEdit { path } => {
                if let Some(open) = self.open.as_mut() {
                    open.files.insert(path.to_string());
                }
            }
        }
    }

    fn task_started(&mut self, turn_id: &str, ts_ms: i64) {
        if let Some(open) = self.open.take() {
            let finished = self.finish(open);
            self.finished.push(finished);
        }
        let slot = std::mem::take(&mut self.slot);
        if !slot.blocks.is_empty() {
            self.user_turns
                .push(slot.record(self.session_id, turn_id, ts_ms));
        }
        let mut content = std::mem::take(&mut self.pending_content);
        for record in &mut content {
            record.message_id = turn_id.to_string();
        }
        self.open = Some(Open {
            turn_id: turn_id.to_string(),
            ts_ms,
            start: self.cumulative,
            usage_observed: false,
            counter_error: None,
            tool_calls: Vec::new(),
            seen_calls: HashSet::new(),
            files: BTreeSet::new(),
            user_text: std::mem::take(&mut self.pending_user_text),
            assistant_text: String::new(),
            content,
            events: Vec::new(),
            relationships: Vec::new(),
            spawns: HashMap::new(),
        });
    }

    fn task_complete(&mut self, line: u64, turn_id: &str) {
        if self.open.as_ref().is_none_or(|o| o.turn_id != turn_id) {
            return;
        }
        let Some(mut open) = self.open.take() else {
            return;
        };
        for block in &mut self.slot.blocks {
            let failed = block
                .tool_use_id
                .as_deref()
                .is_some_and(|id| self.errored.contains(id));
            if block.kind == UserTurnBlockKind::ToolResult && failed {
                block.is_error = Some(true);
            }
        }
        for mut event in std::mem::take(&mut open.events) {
            if self.errored.contains(event.tool_use_id.as_str()) {
                event.status = ToolResultStatus::Errored;
                event.is_error = Some(true);
            } else if event.status == ToolResultStatus::Unknown {
                event.status = ToolResultStatus::Completed;
            }
            self.events.push((line, event));
        }
        for relationship in std::mem::take(&mut open.relationships) {
            self.relationships.push((line, relationship));
        }
        self.slot.preceding_message_id = Some(open.turn_id.clone());
        let (turn, content) = self.finish(open);
        self.last_completed = Some((turn.message_id.clone(), turn.usage.cache_read));
        self.finished.push((turn, content));
        self.committed_turns = self.finished.len();
        self.committed_user_turns = self.user_turns.len();
        self.committed_line = Some(line);
    }

    fn compacted(&mut self, line: u64, ts_ms: i64) {
        let preceding = self.last_completed.as_ref();
        self.compactions.push((
            line,
            CompactionEvent {
                v: 1,
                source: SourceKind::Codex,
                session_id: self.session_id.to_string(),
                ts: format_iso_ms(ts_ms),
                preceding_message_id: preceding.map(|(id, _)| id.clone()),
                tokens_before_compact: preceding.map(|(_, cache_read)| *cache_read),
            },
        ));
    }

    /// Advance the running total. A snapshot that cannot be read, or that
    /// runs backwards, leaves the open task's usage unknown; a regressed
    /// total is still the baseline the next task spends from.
    fn usage_snapshot(&mut self, info: &serde_json::Value) {
        let Some(read) = read_snapshot(info) else {
            return;
        };
        let problem = match read {
            Ok(next) => {
                let problem = spend(&self.cumulative, &next).err();
                self.cumulative = next;
                problem
            }
            Err(error) => Some(error),
        };
        if let Some(open) = self.open.as_mut() {
            open.usage_observed = true;
            if let Some(error) = problem {
                open.counter_error.get_or_insert(error);
            }
        }
    }

    fn user_text(&mut self, text: &str, ts_ms: i64) {
        let target = match self.open.as_mut() {
            Some(open) => &mut open.user_text,
            None => &mut self.pending_user_text,
        };
        append(target, text);
        self.slot
            .push(UserTurnBlock::text(text, &HeuristicCounter), ts_ms);
        let record = |message_id: &str| {
            text_content(
                self.session_id,
                message_id,
                ts_ms,
                ContentRole::User,
                ContentKind::Text,
                text,
            )
        };
        match self.open.as_mut() {
            Some(open) => open.content.push(record(&open.turn_id)),
            None => self.pending_content.push(record("")),
        }
    }

    fn assistant_output(&mut self, text: &str, ts_ms: i64, reasoning: bool) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let kind = if reasoning {
            ContentKind::Thinking
        } else {
            append(&mut open.assistant_text, text);
            ContentKind::Text
        };
        open.content.push(text_content(
            self.session_id,
            &open.turn_id,
            ts_ms,
            ContentRole::Assistant,
            kind,
            text,
        ));
    }

    fn finish(&self, open: Open) -> (TurnRecord, Vec<ContentRecord>) {
        let (usage, usage_known) = match (&open.counter_error, open.usage_observed) {
            (None, true) => match spend(&open.start, &self.cumulative) {
                Ok(usage) => (usage, true),
                Err(_) => (Usage::default(), false),
            },
            _ => (Usage::default(), false),
        };
        let (model, cwd) = self
            .contexts
            .get(open.turn_id.as_str())
            .copied()
            .unwrap_or_default();
        let cwd = cwd.or(self.ev.session.cwd.as_deref());
        let resolved = cwd.map(resolve_project);
        let text = join_nonempty(&[&open.user_text, &open.assistant_text], "\n");
        let classified = classify_activity(ClassificationInput {
            tool_calls: &open.tool_calls,
            text: &text,
            has_failed_tool: open
                .tool_calls
                .iter()
                .any(|c| self.errored.contains(c.id.as_str())),
            reasoning_tokens: usage.reasoning,
        });
        let files: Vec<String> = open.files.into_iter().collect();
        let turn = TurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: self.session_id.to_string(),
            session_path: self
                .ev
                .session
                .raw_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            message_id: open.turn_id,
            turn_index: 0,
            ts: format_iso_ms(open.ts_ms),
            model: model.unwrap_or_default().to_string(),
            project: resolved.as_ref().map(|r| r.project.clone()),
            project_key: resolved.and_then(|r| r.project_key),
            usage,
            tool_calls: open.tool_calls,
            files_touched: (!files.is_empty()).then_some(files),
            subagent: None,
            stop_reason: None,
            activity: Some(classified.activity),
            retries: Some(classified.retries),
            has_edits: Some(classified.has_edits),
            fidelity: Some(Fidelity::new(
                UsageGranularity::PerTurn,
                Coverage {
                    has_input_tokens: usage_known,
                    has_output_tokens: usage_known,
                    has_reasoning_tokens: usage_known,
                    has_cache_read_tokens: usage_known,
                    has_cache_create_tokens: false,
                    has_tool_calls: true,
                    has_tool_result_events: true,
                    has_session_relationships: true,
                    has_raw_content: true,
                },
            )),
        };
        (turn, open.content)
    }

    fn into_derived(mut self) -> Derived {
        let Some(committed_line) = self.committed_line else {
            return Derived::default();
        };
        self.finished.truncate(self.committed_turns);
        self.user_turns.truncate(self.committed_user_turns);
        let mut derived = Derived {
            user_turns: self.user_turns,
            committed: true,
            ..Derived::default()
        };
        for (index, (mut turn, content)) in self.finished.into_iter().enumerate() {
            turn.turn_index = index as u64;
            derived.turns.push(turn);
            derived.content.extend(content);
        }
        derived.tool_result_events = committed(self.events, committed_line);
        derived.subagents = committed(self.relationships, committed_line);
        derived.compactions = committed(self.compactions, committed_line);
        derived
    }
}

/// Records emitted at or before the last commit.
fn committed<T>(records: Vec<(u64, T)>, committed_line: u64) -> Vec<T> {
    records
        .into_iter()
        .filter(|(line, _)| *line <= committed_line)
        .map(|(_, record)| record)
        .collect()
}

fn append(buffer: &mut String, text: &str) {
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(text);
}
