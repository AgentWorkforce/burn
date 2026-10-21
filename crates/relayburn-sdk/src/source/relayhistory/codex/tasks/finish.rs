//! A committed Codex task as burn's turn record.

use super::super::snapshots::spend;
use super::requests::RequestTurn;
use super::{Finished, Open, Tasks};
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::resolve_project;
use crate::reader::types::{
    Coverage, Fidelity, SourceKind, StopReason, TurnRecord, Usage, UsageGranularity,
};
use crate::reader::user_turn::join_nonempty;
use crate::util::time::format_iso_ms;

impl<'a> Tasks<'a> {
    pub(super) fn finish(
        &self,
        open: Open<'a>,
        message_id: String,
        stop_reason: Option<StopReason>,
    ) -> Finished {
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
        let model = model.unwrap_or_default();
        let request_count = open.requests.len() as u64;
        let inferences = RequestTurn {
            session_id: self.session_id,
            message_id: &message_id,
            model,
            usage_known,
        }
        .inferences(open.requests);
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
            request_count: Some(request_count),
            ts: format_iso_ms(open.ts_ms),
            model: model.to_string(),
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
        Finished {
            turn,
            content,
            inferences,
        }
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
}
