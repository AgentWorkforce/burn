//! The model requests inside a Codex task.
//!
//! Codex writes a cumulative `token_count` after every model request, so
//! each snapshot that moves the running total ends one request: the
//! request spent the difference from the previous one, and issued every
//! tool call written since. A snapshot that repeats the running total is
//! a re-report, not a request. The task stays burn's turn; its requests
//! become one inference row each, and their count is the turn's
//! `request_count`.

use super::super::snapshots::{spend, Counters};
use super::Open;
use crate::reader::types::{SourceKind, Usage};
use crate::reader::{Inference, InferenceKeySource, InferenceKind, ToolUseRef};
use crate::util::time::format_iso_ms;

/// One model request of a task, before the task commits.
pub(super) struct Request {
    /// What the request spent; `None` when its snapshot cannot be
    /// accounted against the one before.
    usage: Option<Usage>,
    tool_uses: Vec<ToolUseRef>,
    start_ms: i64,
    end_ms: i64,
}

/// Where the open part of a task is in its request sequence.
#[derive(Default)]
pub(super) struct Requests {
    done: Vec<Request>,
    /// The running total the next request spends from.
    base: Counters,
    /// When the next request's first record was written.
    from_ms: Option<i64>,
    /// Tool calls before this index belong to an earlier request.
    claimed_calls: usize,
}

impl Requests {
    /// The request sequence of a task part whose spend starts at `base`.
    pub(super) fn from(base: Counters) -> Self {
        Self {
            base,
            ..Self::default()
        }
    }

    pub(super) fn len(&self) -> usize {
        self.done.len()
    }
}

impl Open<'_> {
    /// A record that belongs to the request in progress was written.
    pub(super) fn request_activity(&mut self, ts_ms: i64) {
        self.requests.from_ms.get_or_insert(ts_ms);
    }

    /// A readable snapshot moved the running total from `prior` to `next`
    /// at `ts_ms`; a moved total ends a request.
    pub(super) fn snapshot(&mut self, prior: &Counters, next: &Counters, ts_ms: i64) {
        if next == prior {
            return;
        }
        let requests = &mut self.requests;
        let tool_uses = self.tool_calls[requests.claimed_calls..]
            .iter()
            .map(|call| ToolUseRef {
                id: call.id.clone(),
                name: call.name.clone(),
            })
            .collect();
        requests.claimed_calls = self.tool_calls.len();
        requests.done.push(Request {
            usage: spend(&requests.base, next).ok(),
            tool_uses,
            start_ms: requests.from_ms.take().unwrap_or(ts_ms),
            end_ms: ts_ms,
        });
        requests.base = *next;
    }
}

/// The turn a committed task part's requests belong to.
pub(super) struct RequestTurn<'a> {
    pub session_id: &'a str,
    pub message_id: &'a str,
    pub model: &'a str,
    /// Whether the turn's usage is known; when it is not, no request of it
    /// is billed either.
    pub usage_known: bool,
}

impl RequestTurn<'_> {
    /// One inference row per request, keyed by its place in the turn.
    pub(super) fn inferences(&self, requests: Requests) -> Vec<Inference> {
        requests
            .done
            .into_iter()
            .enumerate()
            .map(|(index, request)| {
                let usage = match request.usage {
                    Some(usage) if self.usage_known => usage,
                    _ => Usage::default(),
                };
                Inference {
                    v: 1,
                    source: SourceKind::Codex,
                    session_id: self.session_id.to_string(),
                    request_id: request_id(self.message_id, index),
                    request_id_source: InferenceKeySource::RowSynthetic,
                    turn_id: self.message_id.to_string(),
                    model: self.model.to_string(),
                    kind: request_kind(&usage, !request.tool_uses.is_empty()),
                    usage,
                    tool_uses: request.tool_uses,
                    start_ts: format_iso_ms(request.start_ms),
                    end_ts: format_iso_ms(request.end_ms),
                    start_ms: request.start_ms,
                    end_ms: request.end_ms,
                }
            })
            .collect()
    }
}

/// The key of the `index`th (0-based) request of turn `message_id`.
fn request_id(message_id: &str, index: usize) -> String {
    format!("{message_id}:request:{:06}", index + 1)
}

/// What a request produced: tool calls, reasoning, or a message.
fn request_kind(usage: &Usage, has_tool_uses: bool) -> InferenceKind {
    match (has_tool_uses, usage.reasoning > 0) {
        (true, true) => InferenceKind::Mixed,
        (true, false) => InferenceKind::ToolUse,
        (false, true) if usage.output == 0 => InferenceKind::Reasoning,
        (false, _) => InferenceKind::Message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(output: u64, reasoning: u64) -> Usage {
        Usage {
            output,
            reasoning,
            ..Usage::default()
        }
    }

    #[test]
    fn request_kind_covers_each_shape() {
        assert_eq!(request_kind(&usage(0, 2), false), InferenceKind::Reasoning);
        assert_eq!(request_kind(&usage(1, 2), false), InferenceKind::Message);
        assert_eq!(request_kind(&usage(0, 0), false), InferenceKind::Message);
        assert_eq!(request_kind(&usage(1, 2), true), InferenceKind::Mixed);
        assert_eq!(request_kind(&usage(0, 2), true), InferenceKind::Mixed);
        assert_eq!(request_kind(&usage(1, 0), true), InferenceKind::ToolUse);
    }

    #[test]
    fn request_ids_number_requests_from_one() {
        assert_eq!(request_id("t1", 0), "t1:request:000001");
        assert_eq!(request_id("t1@12", 41), "t1@12:request:000042");
    }
}
