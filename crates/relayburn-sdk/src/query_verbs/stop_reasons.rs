use super::*;

/// Per-outcome turn counts, surfaced by `burn summary` for the one-line
/// outcome breakdown (`142 end_turn, 3 max_tokens, 1 refusal, 0 pause`).
///
/// Counts mirror the [`StopReason`] enum variants plus a `none` slot for
/// turns whose row carried no `stop_reason` field at all — that's Codex
/// today (no field in the rollout schema) and any pre-3.0 ledger row that
/// was ingested before the reader started populating the enum.
///
/// `Silent` is reserved for "row exists, carries a stop_reason that we
/// don't recognize" — distinct from `none` so we can spot a future harness
/// regression rather than silently lumping it with Codex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReasonCounts {
    pub end_turn: u64,
    pub max_tokens: u64,
    pub pause_turn: u64,
    pub stop_sequence: u64,
    pub tool_use: u64,
    pub refusal: u64,
    pub silent: u64,
    /// Turns whose record carried no `stop_reason` field — e.g. Codex
    /// rollouts (the harness doesn't report one) or pre-3.0 ledger rows
    /// from before the reader started parsing the field.
    pub none: u64,
}

impl StopReasonCounts {
    /// Accumulate one turn's outcome into the bucket counts. `None` lands
    /// in [`Self::none`]; unrecognized variants would already be normalized
    /// to [`StopReason::Silent`] upstream by the lenient deserializer.
    pub fn bump(&mut self, reason: Option<StopReason>) {
        match reason {
            None => self.none += 1,
            Some(StopReason::EndTurn) => self.end_turn += 1,
            Some(StopReason::MaxTokens) => self.max_tokens += 1,
            Some(StopReason::PauseTurn) => self.pause_turn += 1,
            Some(StopReason::StopSequence) => self.stop_sequence += 1,
            Some(StopReason::ToolUse) => self.tool_use += 1,
            Some(StopReason::Refusal) => self.refusal += 1,
            Some(StopReason::Silent) => self.silent += 1,
        }
    }

    /// Fold every turn's `stop_reason` into a fresh counts struct.
    pub fn from_turns(turns: &[TurnRecord]) -> Self {
        let mut out = Self::default();
        for t in turns {
            out.bump(t.stop_reason);
        }
        out
    }

    /// True iff every counter is zero — useful for "skip the outcome line
    /// entirely" presentation logic in summary.
    pub fn is_empty(&self) -> bool {
        self.end_turn
            | self.max_tokens
            | self.pause_turn
            | self.stop_sequence
            | self.tool_use
            | self.refusal
            | self.silent
            | self.none
            == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{SourceKind, Usage};

    const ALL_REASONS: [Option<StopReason>; 8] = [
        None,
        Some(StopReason::EndTurn),
        Some(StopReason::MaxTokens),
        Some(StopReason::PauseTurn),
        Some(StopReason::StopSequence),
        Some(StopReason::ToolUse),
        Some(StopReason::Refusal),
        Some(StopReason::Silent),
    ];

    fn counts_with(reasons: &[Option<StopReason>]) -> StopReasonCounts {
        let mut counts = StopReasonCounts::default();
        for reason in reasons {
            counts.bump(*reason);
        }
        counts
    }

    #[test]
    fn bump_routes_each_reason_to_its_own_bucket() {
        let counts = counts_with(&[
            None,
            Some(StopReason::EndTurn),
            Some(StopReason::EndTurn),
            Some(StopReason::MaxTokens),
            Some(StopReason::MaxTokens),
            Some(StopReason::MaxTokens),
            Some(StopReason::PauseTurn),
            Some(StopReason::StopSequence),
            Some(StopReason::StopSequence),
            Some(StopReason::ToolUse),
            Some(StopReason::ToolUse),
            Some(StopReason::ToolUse),
            Some(StopReason::ToolUse),
            Some(StopReason::Refusal),
            Some(StopReason::Refusal),
            Some(StopReason::Silent),
            Some(StopReason::Silent),
            Some(StopReason::Silent),
        ]);
        assert_eq!(
            counts,
            StopReasonCounts {
                end_turn: 2,
                max_tokens: 3,
                pause_turn: 1,
                stop_sequence: 2,
                tool_use: 4,
                refusal: 2,
                silent: 3,
                none: 1,
            }
        );
    }

    #[test]
    fn from_turns_folds_every_turn_stop_reason() {
        let turns: Vec<TurnRecord> = [Some(StopReason::EndTurn), None, Some(StopReason::EndTurn)]
            .into_iter()
            .enumerate()
            .map(|(index, stop_reason)| TurnRecord {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "s".to_string(),
                message_id: format!("m-{index}"),
                turn_index: index as u64,
                ts: "2026-07-30T00:00:00.000Z".to_string(),
                model: "test-model".to_string(),
                session_path: None,
                project: None,
                project_key: None,
                usage: Usage::default(),
                tool_calls: Vec::new(),
                files_touched: None,
                subagent: None,
                stop_reason,
                activity: None,
                retries: None,
                has_edits: None,
                fidelity: None,
            })
            .collect();
        assert_eq!(
            StopReasonCounts::from_turns(&turns),
            StopReasonCounts {
                end_turn: 2,
                none: 1,
                ..StopReasonCounts::default()
            }
        );
        assert_eq!(
            StopReasonCounts::from_turns(&[]),
            StopReasonCounts::default()
        );
    }

    #[test]
    fn is_empty_is_true_only_when_every_bucket_is_zero() {
        assert!(StopReasonCounts::default().is_empty());
        for reason in ALL_REASONS {
            assert!(!counts_with(&[reason]).is_empty(), "{reason:?}");
        }
        assert!(!counts_with(&ALL_REASONS).is_empty());
    }

    #[test]
    fn is_empty_is_false_for_any_two_adjacent_buckets() {
        // Field order of the OR chain in `is_empty`.
        let chain_order = [
            Some(StopReason::EndTurn),
            Some(StopReason::MaxTokens),
            Some(StopReason::PauseTurn),
            Some(StopReason::StopSequence),
            Some(StopReason::ToolUse),
            Some(StopReason::Refusal),
            Some(StopReason::Silent),
            None,
        ];
        for pair in chain_order.windows(2) {
            assert!(!counts_with(pair).is_empty(), "{pair:?}");
        }
    }
}
