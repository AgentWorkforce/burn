//! Burn's Codex turn is the task: `task_started` opens it, and it commits
//! at the first of its `task_complete`, its `turn_aborted`, or the next
//! `task_started`. The provider charged for an aborted or abandoned task
//! as much as for a completed one, so each is billed.
//!
//! A task still open at the end of the rollout commits once the rollout has
//! settled: nothing has been written to it for [`TAIL_GRACE_MS`]. Until
//! then it is pending and nothing it derives is emitted; one that never
//! reported spend bills nothing. A task billed open
//! is a segment `{turn_id}@{line}` that ends at the last line read then;
//! the ledger holds that id, so when the task continues anyway its later
//! spend is billed from that line on as a turn of its own, never re-billed
//! under the first segment's id and never lost.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem::take;

use ai_hist::SessionEvidence;

#[cfg(test)]
mod tests;
mod tools;

use super::context::{Chain, TurnContext};
use super::events::{Event, Located};
use super::records::{text_content, Slot, Spawn};
use super::snapshots::{read_snapshot, spend, CounterError, Counters};
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::resolve_project;
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, Coverage, Fidelity,
    SessionRelationshipRecord, SourceKind, StopReason, ToolCall, ToolResultEventRecord,
    ToolResultStatus, TurnRecord, Usage, UsageGranularity, UserTurnBlock, UserTurnBlockKind,
    UserTurnRecord,
};
use crate::reader::user_turn::join_nonempty;
use crate::source::{Billing, TAIL_GRACE_MS};
use crate::util::time::format_iso_ms;

/// The message id of the part of task `turn_id` billed through `line`.
fn segment_id(turn_id: &str, line: u64) -> String {
    format!("{turn_id}@{line}")
}

/// Per task, the lines its billed segments end at, in order.
fn billed_cuts(billed: &HashSet<String>) -> HashMap<&str, Vec<u64>> {
    let mut cuts: HashMap<&str, Vec<u64>> = HashMap::new();
    for id in billed {
        if let Some((turn_id, line)) = id.rsplit_once('@') {
            if let Ok(line) = line.parse() {
                cuts.entry(turn_id).or_default().push(line);
            }
        }
    }
    for lines in cuts.values_mut() {
        lines.sort_unstable();
    }
    cuts
}

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
    /// Whether an open trailing task waits for the rollout to settle.
    pub pending: bool,
}

/// The task being read: the part of it after the last billed segment.
struct Open<'a> {
    turn_id: String,
    ts_ms: i64,
    /// Lines the ledger's billed segments of this task end at, still ahead.
    cuts: Vec<u64>,
    /// This part follows a billed segment of the same task.
    continuation: bool,
    /// Events that bill or describe this part of the task.
    activity: usize,
    /// The user turn that opened the task, until its first part commits.
    user_turn: Option<usize>,
    /// The task's `turn_context`; `None` when none names or precedes it.
    context: Option<TurnContext<'a>>,
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
    now_ms: i64,
    cuts: HashMap<&'a str, Vec<u64>>,
    /// The last rollout line read, and when anything was last written.
    last_line: u64,
    last_activity_ms: i64,
    pending: bool,
    /// Calls Codex reported failed (`exec_command_end` exit code,
    /// `patch_apply_end` success).
    errored: HashSet<&'a str>,
    chain: Chain<'a>,
    /// turn id → (model, cwd) as stamped on its messages.
    stamps: HashMap<&'a str, (Option<&'a str>, Option<&'a str>)>,
    cumulative: Counters,
    open: Option<Open<'a>>,
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
    pub(super) fn new(ev: &'a SessionEvidence, billing: &'a Billing) -> Self {
        let mut stamps: HashMap<&str, (Option<&str>, Option<&str>)> = HashMap::new();
        for m in &ev.messages {
            if let Some(turn) = m.turn_id.as_deref() {
                let slot = stamps.entry(turn).or_default();
                slot.0 = slot.0.or(m.model.as_deref());
                slot.1 = slot.1.or(m.cwd.as_deref());
            }
        }
        Self {
            ev,
            session_id: &ev.session.session_id,
            now_ms: billing.now_ms,
            cuts: billed_cuts(&billing.billed),
            last_line: 0,
            last_activity_ms: ev.session.last_activity_ms.unwrap_or_default(),
            pending: false,
            errored: ev
                .tool_calls
                .iter()
                .filter(|c| c.is_error == Some(true))
                .map(|c| c.tool_use_id.as_str())
                .collect(),
            chain: Chain::new(ev),
            stamps,
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
            self.observe(located);
            self.apply(located.line, &located.event);
        }
        self.settle_tail();
        self.into_derived()
    }

    /// Track where the rollout got to, and cut the open task where the
    /// ledger's billed segments of it end.
    fn observe(&mut self, located: &Located<'a>) {
        if located.line != u64::MAX {
            self.last_line = self.last_line.max(located.line);
        }
        if let Some(ts_ms) = located.ts_ms {
            self.last_activity_ms = self.last_activity_ms.max(ts_ms);
        }
        while let Some(cut) = self.next_cut().filter(|cut| *cut < located.line) {
            self.cut(cut, located.ts_ms);
        }
        if let Some(open) = self.open.as_mut() {
            open.activity += usize::from(located.event.is_activity());
        }
    }

    fn next_cut(&self) -> Option<u64> {
        self.open.as_ref()?.cuts.first().copied()
    }

    /// Commit the open task through `cut`, and keep reading it after.
    fn cut(&mut self, cut: u64, ts_ms: Option<i64>) {
        let Some(mut open) = self.open.take() else {
            return;
        };
        let rest = Open {
            turn_id: open.turn_id.clone(),
            ts_ms: ts_ms.unwrap_or(open.ts_ms),
            cuts: open.cuts.split_off(1),
            continuation: true,
            activity: 0,
            user_turn: None,
            context: open.context,
            start: self.cumulative,
            usage_observed: open.usage_observed,
            counter_error: None,
            tool_calls: Vec::new(),
            seen_calls: take(&mut open.seen_calls),
            files: BTreeSet::new(),
            user_text: String::new(),
            assistant_text: String::new(),
            content: Vec::new(),
            events: Vec::new(),
            relationships: Vec::new(),
            spawns: take(&mut open.spawns),
        };
        let id = segment_id(&open.turn_id, cut);
        self.commit(cut, open, id, None);
        self.open = Some(rest);
    }

    /// The open task at the end of the rollout: billed once the rollout
    /// has settled, pending until then; one that settled without reporting
    /// any spend bills nothing. What the ledger already bills of it is
    /// emitted either way.
    fn settle_tail(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        let line = self.last_line;
        let settled = self.now_ms.saturating_sub(self.last_activity_ms) >= TAIL_GRACE_MS;
        if !open.cuts.is_empty() || open.is_empty() {
            self.close(line, open, None);
        } else if settled && !open.usage_observed {
            // A task that never reported spend has nothing to bill, and
            // nothing it derived is emitted.
        } else if settled {
            let id = segment_id(&open.turn_id, line);
            self.commit(line, open, id, None);
        } else {
            self.pending = true;
        }
    }

    fn apply(&mut self, line: u64, event: &Event<'a>) {
        match event {
            Event::TaskStarted { turn_id, ts_ms } => self.task_started(line, turn_id, *ts_ms),
            Event::TaskComplete { turn_id } => self.task_ended(line, turn_id, None),
            Event::TaskAborted { turn_id } => {
                self.task_ended(line, turn_id, Some(StopReason::Aborted));
            }
            Event::Compacted { ts_ms } => self.compacted(line, *ts_ms),
            Event::TurnContext { payload } => self.chain.observe(payload),
            Event::UsageSnapshot { snapshot } => self.usage_snapshot(snapshot),
            Event::InheritedTotal { counters } => self.cumulative = *counters,
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

    fn task_started(&mut self, line: u64, turn_id: &str, ts_ms: i64) {
        if let Some(open) = self.open.take() {
            self.close(line, open, None);
        }
        let slot = take(&mut self.slot);
        let mut user_turn = None;
        if !slot.blocks.is_empty() {
            user_turn = Some(self.user_turns.len());
            self.user_turns
                .push(slot.record(self.session_id, turn_id, ts_ms));
        }
        let content = take(&mut self.pending_content);
        self.open = Some(Open {
            turn_id: turn_id.to_string(),
            ts_ms,
            cuts: self.cuts.remove(turn_id).unwrap_or_default(),
            continuation: false,
            activity: 0,
            user_turn,
            context: self.chain.for_turn(turn_id),
            start: self.cumulative,
            usage_observed: false,
            counter_error: None,
            tool_calls: Vec::new(),
            seen_calls: HashSet::new(),
            files: BTreeSet::new(),
            user_text: take(&mut self.pending_user_text),
            assistant_text: String::new(),
            content,
            events: Vec::new(),
            relationships: Vec::new(),
            spawns: HashMap::new(),
        });
    }

    fn task_ended(&mut self, line: u64, turn_id: &str, stop: Option<StopReason>) {
        if self.open.as_ref().is_none_or(|o| o.turn_id != turn_id) {
            return;
        }
        if let Some(open) = self.open.take() {
            self.close(line, open, stop);
        }
    }

    /// Commit the open task at `line`: under the id of a billed segment it
    /// has not passed, else under its own. A continuation nothing happened
    /// in bills nothing.
    fn close(&mut self, line: u64, open: Open<'a>, stop: Option<StopReason>) {
        if let Some(cut) = open.cuts.first() {
            let id = segment_id(&open.turn_id, *cut);
            self.commit(line, open, id, stop);
        } else if open.is_empty() {
            self.mark_committed(line);
        } else {
            let id = open.turn_id.clone();
            self.commit(line, open, id, stop);
        }
    }

    fn commit(
        &mut self,
        line: u64,
        mut open: Open<'a>,
        message_id: String,
        stop: Option<StopReason>,
    ) {
        for block in &mut self.slot.blocks {
            let failed = block
                .tool_use_id
                .as_deref()
                .is_some_and(|id| self.errored.contains(id));
            if block.kind == UserTurnBlockKind::ToolResult && failed {
                block.is_error = Some(true);
            }
        }
        for mut event in take(&mut open.events) {
            if self.errored.contains(event.tool_use_id.as_str()) {
                event.status = ToolResultStatus::Errored;
                event.is_error = Some(true);
            } else if event.status == ToolResultStatus::Unknown {
                event.status = ToolResultStatus::Completed;
            }
            event.message_id = Some(message_id.clone());
            self.events.push((line, event));
        }
        for relationship in take(&mut open.relationships) {
            self.relationships.push((line, relationship));
        }
        if let Some(index) = open.user_turn.take() {
            self.user_turns[index].following_message_id = Some(message_id.clone());
        }
        self.slot.preceding_message_id = Some(message_id.clone());
        let (turn, content) = self.finish(open, message_id, stop);
        self.last_completed = Some((turn.message_id.clone(), turn.usage.cache_read));
        self.finished.push((turn, content));
        self.mark_committed(line);
    }

    fn mark_committed(&mut self, line: u64) {
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
    fn usage_snapshot(&mut self, snapshot: &ai_hist::UsageSnapshot) {
        let Some(read) = read_snapshot(snapshot) else {
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
        self.slot.push(UserTurnBlock::text(text), ts_ms);
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

    fn finish(
        &self,
        open: Open<'a>,
        message_id: String,
        stop_reason: Option<StopReason>,
    ) -> (TurnRecord, Vec<ContentRecord>) {
        let (usage, usage_known) = match (&open.counter_error, open.usage_observed) {
            (None, true) => match spend(&open.start, &self.cumulative) {
                Ok(usage) => (usage, true),
                Err(_) => (Usage::default(), false),
            },
            _ => (Usage::default(), false),
        };
        let (model, cwd) = self.configuration(&open);
        let resolved = cwd.or(self.ev.session.cwd.as_deref()).map(resolve_project);
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
        let mut content = open.content;
        for record in &mut content {
            record.message_id.clone_from(&message_id);
        }
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
            message_id,
            turn_index: 0,
            ts: format_iso_ms(open.ts_ms),
            model: model.unwrap_or_default().to_string(),
            project: resolved.as_ref().map(|r| r.project.clone()),
            project_key: resolved.and_then(|r| r.project_key),
            usage,
            tool_calls: open.tool_calls,
            files_touched: (!files.is_empty()).then_some(files),
            subagent: None,
            stop_reason,
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
            reasoning: open.context.and_then(|c| c.reasoning()),
        };
        (turn, content)
    }

    /// The task's model and cwd: its `turn_context`'s, else — only when no
    /// `turn_context` names or precedes it — what its messages carry.
    fn configuration(&self, open: &Open<'a>) -> (Option<&'a str>, Option<&'a str>) {
        match open.context {
            Some(context) => (context.model, context.cwd),
            None => self
                .stamps
                .get(open.turn_id.as_str())
                .copied()
                .unwrap_or_default(),
        }
    }

    fn into_derived(mut self) -> Derived {
        let Some(committed_line) = self.committed_line else {
            return Derived {
                pending: self.pending,
                ..Derived::default()
            };
        };
        self.finished.truncate(self.committed_turns);
        self.user_turns.truncate(self.committed_user_turns);
        let mut derived = Derived {
            user_turns: self.user_turns,
            committed: true,
            pending: self.pending,
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

impl Open<'_> {
    /// A continuation nothing has happened in yet.
    fn is_empty(&self) -> bool {
        self.continuation && self.activity == 0
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
