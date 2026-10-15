//! The coarse outcome of an assistant turn and its lenient wire decoding.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Coarse outcome of an assistant turn, derived from the harness-reported
/// stop reason on the trailing assistant row.
///
/// Wire shape is kebab-case (`end-turn`, `max-tokens`, …). On-disk and on the
/// JSONL surface this is round-trippable as a string; absent rows decode as
/// `None`. `Silent` is reserved for the "we have an inference but it carries
/// no stop reason" case (mid-write, sidechain, harness that doesn't report
/// one in the row we parsed). The lossy mapping for non-Anthropic harnesses
/// is intentional; downstream presenters consume the variant, not the raw
/// string. See issue #437.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    PauseTurn,
    StopSequence,
    ToolUse,
    Refusal,
    /// The harness ended the turn on an interrupt or cancellation (Codex
    /// `turn_aborted`). Its spend is billed like any other turn's.
    Aborted,
    Silent,
}

impl StopReason {
    /// Kebab-case label as emitted on the wire (matches
    /// `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end-turn",
            Self::MaxTokens => "max-tokens",
            Self::PauseTurn => "pause-turn",
            Self::StopSequence => "stop-sequence",
            Self::ToolUse => "tool-use",
            Self::Refusal => "refusal",
            Self::Aborted => "aborted",
            Self::Silent => "silent",
        }
    }

    /// Map a harness-emitted stop-reason string (e.g. Anthropic's
    /// `end_turn` / `max_tokens` or opencode's `tool-calls`) onto the
    /// canonical [`StopReason`]. Returns `None` for unrecognized inputs so
    /// callers can decide whether to fall back to [`StopReason::Silent`]
    /// or keep `None`.
    ///
    /// Matching is case-insensitive and accepts either snake_case or
    /// kebab-case so the same parser handles Anthropic (`max_tokens`),
    /// opencode (`tool-calls`), and the kebab-case wire form we emit
    /// ourselves on round-trip.
    pub fn from_wire(raw: &str) -> Option<Self> {
        let normalized: String = raw
            .trim()
            .chars()
            .map(|c| match c {
                '_' => '-',
                c => c.to_ascii_lowercase(),
            })
            .collect();
        match normalized.as_str() {
            // OpenAI / AI-SDK convention emits `"stop"` for ordinary
            // end-of-turn completions (this is what opencode forwards),
            // so it maps to `EndTurn`, not `StopSequence`. Anthropic's
            // actual stop-sequence outcome is the explicit
            // `"stop_sequence"` / `"stop-sequence"` variant below.
            "end-turn" | "stop" => Some(Self::EndTurn),
            "max-tokens" | "length" => Some(Self::MaxTokens),
            "pause-turn" => Some(Self::PauseTurn),
            "stop-sequence" => Some(Self::StopSequence),
            // `tool-calls` is the opencode AI-SDK wire form; `tool-use`
            // is the Anthropic one and our canonical kebab spelling.
            "tool-use" | "tool-calls" => Some(Self::ToolUse),
            "refusal" => Some(Self::Refusal),
            "aborted" => Some(Self::Aborted),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
}

/// Lenient deserializer for `TurnRecord.stop_reason`. Accepts the canonical
/// kebab-case variant (`end-turn`, `max-tokens`, …) plus the legacy free-text
/// shapes from upstream harnesses (`end_turn`, `tool_use`, opencode's
/// `tool-calls`, etc.). An unrecognized string decodes to
/// [`StopReason::Silent`] instead of an error so a pre-3.0 ledger replays
/// cleanly through the new column.
pub(super) fn deserialize_optional_stop_reason<'de, D>(
    d: D,
) -> std::result::Result<Option<StopReason>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(d)?;
    Ok(opt.map(|s| StopReason::from_wire(&s).unwrap_or(StopReason::Silent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [StopReason; 8] = [
        StopReason::EndTurn,
        StopReason::MaxTokens,
        StopReason::PauseTurn,
        StopReason::StopSequence,
        StopReason::ToolUse,
        StopReason::Refusal,
        StopReason::Aborted,
        StopReason::Silent,
    ];

    #[test]
    fn the_wire_spelling_round_trips_and_matches_serde_and_display() {
        for reason in ALL {
            let wire = reason.wire_str();
            assert_eq!(StopReason::from_wire(wire), Some(reason), "{wire}");
            assert_eq!(reason.to_string(), wire);
            assert_eq!(serde_json::to_value(reason).unwrap(), wire);
        }
    }
}
