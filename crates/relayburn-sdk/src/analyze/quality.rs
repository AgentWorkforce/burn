//! Quality signals for the "was this work good enough that a cheaper model
//! could have done it" question. Rust port of `packages/analyze/src/quality.ts`.
//!
//! Two orthogonal detectors: outcome inference (agentsview) + one-shot rate
//! (codeburn).
//!
//! Design choices (preserved from TS):
//! - No prompt storage required — both signals work from session metadata and
//!   tool-call patterns alone. Content (last assistant text) is used *only*
//!   to downgrade confidence; never required.
//! - Computed lazily at query time, not persisted in the ledger. Upgrading
//!   the rules later doesn't require a rebuild.
//! - Confidence is explicit on every classification so downstream consumers
//!   can filter out low-confidence signals rather than treat them as noise.

use std::collections::HashMap;

use crate::reader::{ContentKind, ContentRecord, ContentRole, TurnRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeLabel {
    Completed,
    Abandoned,
    Errored,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeReason {
    Automated,
    SingleExchange,
    TooShort,
    Recent,
    UserEnded,
    UserEndedLong,
    FailureStreak,
    GiveUp,
    AssistantEnded,
    UnknownEnding,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOutcome {
    pub session_id: String,
    pub outcome: OutcomeLabel,
    pub confidence: OutcomeConfidence,
    pub is_recent: bool,
    pub reason: OutcomeReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OneShotMetrics {
    pub session_id: String,
    pub edit_turns: u64,
    pub one_shot_turns: u64,
    /// `oneShotTurns / editTurns` when editTurns > 0, else `None`. Callers
    /// decide what to display for zero-edit sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_shot_rate: Option<f64>,
    /// Total retries across all turns in the session.
    pub total_retries: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityResult {
    pub outcomes: Vec<SessionOutcome>,
    pub one_shot: Vec<OneShotMetrics>,
}

#[derive(Debug, Clone, Default)]
pub struct ComputeQualityOptions<'a> {
    /// Optional content sidecar records. When provided, give-up phrase
    /// matching on the last assistant text downgrades assistant-ended
    /// sessions from `completed/medium` to `completed/low`. Without content,
    /// the give-up downgrade is skipped — the classifier still runs.
    pub content_by_session: Option<&'a HashMap<String, Vec<ContentRecord>>>,
    /// Clock override for tests, in milliseconds since the Unix epoch.
    /// Defaults to system clock when `None`.
    pub now_ms: Option<i64>,
}

const GIVE_UP_PATTERNS: &[&str] = &[
    "i'm unable to",
    "i am unable to",
    "i can't proceed",
    "i cannot proceed",
    "i don't have access",
    "i cannot access",
    "unable to verify",
    "doesn't appear to exist",
];

const RECENT_WINDOW_MS: i64 = 10 * 60 * 1000;
const SHORT_CONVERSATION_THRESHOLD: usize = 3;
const LONG_CONVERSATION_THRESHOLD: usize = 10;
const FAILURE_STREAK_THRESHOLD: u64 = 3;

pub fn compute_quality(turns: &[TurnRecord], opts: &ComputeQualityOptions) -> QualityResult {
    // Preserve TS Map iteration order: insertion-order across sessionIds.
    // Borrow rather than clone — nothing here mutates the turns and the
    // input slice outlives every per-session aggregation.
    let mut by_session: Vec<(String, Vec<&TurnRecord>)> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for t in turns {
        if let Some(&i) = idx.get(&t.session_id) {
            by_session[i].1.push(t);
        } else {
            idx.insert(t.session_id.clone(), by_session.len());
            by_session.push((t.session_id.clone(), vec![t]));
        }
    }

    let now = opts.now_ms.unwrap_or_else(now_ms_system);

    let mut outcomes = Vec::with_capacity(by_session.len());
    let mut one_shot = Vec::with_capacity(by_session.len());
    for (session_id, mut session_turns) in by_session {
        session_turns.sort_by_key(|t| t.turn_index);
        outcomes.push(infer_outcome_refs(
            &session_id,
            &session_turns,
            opts.content_by_session,
            now,
        ));
        one_shot.push(compute_one_shot_rate_refs(&session_id, &session_turns));
    }

    QualityResult { outcomes, one_shot }
}

#[cfg(test)]
pub fn infer_outcome(
    session_id: &str,
    turns: &[TurnRecord],
    content_by_session: Option<&HashMap<String, Vec<ContentRecord>>>,
    now_ms: i64,
) -> SessionOutcome {
    let refs: Vec<&TurnRecord> = turns.iter().collect();
    infer_outcome_refs(session_id, &refs, content_by_session, now_ms)
}

fn infer_outcome_refs(
    session_id: &str,
    turns: &[&TurnRecord],
    content_by_session: Option<&HashMap<String, Vec<ContentRecord>>>,
    now_ms: i64,
) -> SessionOutcome {
    if turns.is_empty() {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Unknown,
            confidence: OutcomeConfidence::Low,
            is_recent: false,
            reason: OutcomeReason::Empty,
        };
    }

    let last = turns.last().unwrap();
    let last_ms = parse_iso8601_ms(&last.ts);
    let is_recent = match last_ms {
        Some(ms) => now_ms - ms < RECENT_WINDOW_MS,
        None => false,
    };

    let message_count = turns.len();
    let ended_role = ending_role(turns);
    let failure_streak = trailing_failure_streak(turns);

    // A single assistant turn that reached end_turn is almost always an
    // intentional one-shot exchange. TurnRecord counts assistant turns only,
    // so messageCount <= 2 covers both shapes.
    if message_count <= 2 && ended_role == EndingRole::Assistant {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Completed,
            confidence: OutcomeConfidence::Medium,
            is_recent,
            reason: OutcomeReason::SingleExchange,
        };
    }
    if message_count < SHORT_CONVERSATION_THRESHOLD {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Unknown,
            confidence: OutcomeConfidence::Low,
            is_recent,
            reason: OutcomeReason::TooShort,
        };
    }
    if is_recent {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Unknown,
            confidence: OutcomeConfidence::Low,
            is_recent: true,
            reason: OutcomeReason::Recent,
        };
    }

    if ended_role == EndingRole::User {
        let high = message_count >= LONG_CONVERSATION_THRESHOLD;
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Abandoned,
            confidence: if high {
                OutcomeConfidence::High
            } else {
                OutcomeConfidence::Medium
            },
            is_recent: false,
            reason: if high {
                OutcomeReason::UserEndedLong
            } else {
                OutcomeReason::UserEnded
            },
        };
    }

    if failure_streak >= FAILURE_STREAK_THRESHOLD {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Errored,
            confidence: OutcomeConfidence::Medium,
            is_recent: false,
            reason: OutcomeReason::FailureStreak,
        };
    }

    if ended_role == EndingRole::Unknown {
        return SessionOutcome {
            session_id: session_id.to_string(),
            outcome: OutcomeLabel::Completed,
            confidence: OutcomeConfidence::Low,
            is_recent: false,
            reason: OutcomeReason::UnknownEnding,
        };
    }

    let gave_up = match content_by_session {
        Some(map) => detect_give_up(map.get(session_id)),
        None => false,
    };
    SessionOutcome {
        session_id: session_id.to_string(),
        outcome: OutcomeLabel::Completed,
        confidence: if gave_up {
            OutcomeConfidence::Low
        } else {
            OutcomeConfidence::Medium
        },
        is_recent: false,
        reason: if gave_up {
            OutcomeReason::GiveUp
        } else {
            OutcomeReason::AssistantEnded
        },
    }
}

#[cfg(test)]
pub fn compute_one_shot_rate(session_id: &str, turns: &[TurnRecord]) -> OneShotMetrics {
    let refs: Vec<&TurnRecord> = turns.iter().collect();
    compute_one_shot_rate_refs(session_id, &refs)
}

fn compute_one_shot_rate_refs(session_id: &str, turns: &[&TurnRecord]) -> OneShotMetrics {
    let mut edit_turns: u64 = 0;
    let mut one_shot_turns: u64 = 0;
    let mut total_retries: u64 = 0;
    for t in turns {
        if let Some(s) = &t.subagent {
            if s.is_sidechain {
                continue;
            }
        }
        if !t.has_edits.unwrap_or(false) {
            continue;
        }
        edit_turns += 1;
        let r = t.retries.unwrap_or(0);
        total_retries += r;
        if r == 0 {
            one_shot_turns += 1;
        }
    }
    OneShotMetrics {
        session_id: session_id.to_string(),
        edit_turns,
        one_shot_turns,
        one_shot_rate: if edit_turns > 0 {
            Some(one_shot_turns as f64 / edit_turns as f64)
        } else {
            None
        },
        total_retries,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndingRole {
    User,
    Assistant,
    Unknown,
}

fn ending_role(turns: &[&TurnRecord]) -> EndingRole {
    // TurnRecord represents assistant turns; "ended-with-assistant" means the
    // final turn reached a natural stop (`end_turn`). A non-`end_turn` stop
    // reason means user-ended (session died after a tool_use). When the
    // source doesn't record stopReason at all (e.g. Codex), return Unknown.
    let last = turns.last().expect("turns non-empty");
    match &last.stop_reason {
        None => EndingRole::Unknown,
        Some(s) if s == "end_turn" => EndingRole::Assistant,
        Some(_) => EndingRole::User,
    }
}

fn trailing_failure_streak(turns: &[&TurnRecord]) -> u64 {
    let mut streak: u64 = 0;
    for t in turns.iter().rev() {
        let calls = &t.tool_calls;
        if calls.is_empty() {
            break;
        }
        let all_errored = calls.iter().all(|c| c.is_error == Some(true));
        if !all_errored {
            break;
        }
        streak += calls.len() as u64;
    }
    streak
}

fn detect_give_up(records: Option<&Vec<ContentRecord>>) -> bool {
    let records = match records {
        Some(r) if !r.is_empty() => r,
        _ => return false,
    };
    for r in records.iter().rev() {
        if r.role == ContentRole::Assistant && r.kind == ContentKind::Text {
            if let Some(text) = &r.text {
                let haystack = text.to_lowercase();
                return GIVE_UP_PATTERNS.iter().any(|p| haystack.contains(p));
            }
        }
    }
    false
}

fn now_ms_system() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse a subset of ISO 8601 sufficient for the timestamps produced by the
/// readers and the test fixtures: `YYYY-MM-DDTHH:MM:SS(.fff)?(Z|±HH:MM)`.
/// Returns milliseconds since the Unix epoch, or `None` when the input fails
/// to match — mirroring `Number.isFinite(Date.parse(...))` in TS.
fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i32 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    if bytes[4] != b'-' {
        return None;
    }
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    if bytes[13] != b':' {
        return None;
    }
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    if bytes[16] != b':' {
        return None;
    }
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    // Reject out-of-range components — `Date.parse` returns NaN for these in
    // the TS reference implementation. Day-of-month bounds are validated
    // loosely (1..=31) here; the proleptic-Gregorian conversion below would
    // otherwise silently roll an invalid date forward.
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut idx = 19;
    let mut millis: u32 = 0;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let frac = std::str::from_utf8(&bytes[frac_start..idx]).ok()?;
        // Pad/truncate to 3 digits to get milliseconds.
        let mut buf = String::with_capacity(3);
        for c in frac.chars().take(3) {
            buf.push(c);
        }
        while buf.len() < 3 {
            buf.push('0');
        }
        millis = buf.parse().ok()?;
    }

    let mut tz_offset_minutes: i64 = 0;
    if idx < bytes.len() {
        match bytes[idx] {
            b'Z' | b'z' => {
                idx += 1;
            }
            b'+' | b'-' => {
                let sign: i64 = if bytes[idx] == b'-' { -1 } else { 1 };
                idx += 1;
                if idx + 5 > bytes.len() || bytes[idx + 2] != b':' {
                    return None;
                }
                let oh: i64 = std::str::from_utf8(&bytes[idx..idx + 2])
                    .ok()?
                    .parse()
                    .ok()?;
                let om: i64 = std::str::from_utf8(&bytes[idx + 3..idx + 5])
                    .ok()?
                    .parse()
                    .ok()?;
                tz_offset_minutes = sign * (oh * 60 + om);
                idx += 5;
            }
            _ => return None,
        }
    }
    if idx != bytes.len() {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let utc_secs = days * 86400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let secs = utc_secs - tz_offset_minutes * 60;
    secs.checked_mul(1000)?.checked_add(millis as i64)
}

/// Howard Hinnant's `days_from_civil`: signed days from 1970-01-01 to the
/// given proleptic-Gregorian date.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era: i64 = if y >= 0 {
        (y / 400) as i64
    } else {
        ((y - 399) / 400) as i64
    };
    let yoe: i64 = (y as i64) - era * 400;
    let m_adj: i64 = if m > 2 {
        (m as i64) - 3
    } else {
        (m as i64) + 9
    };
    let doy: i64 = (153 * m_adj + 2) / 5 + (d as i64) - 1;
    let doe: i64 = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{
        ContentKind, ContentRecord, ContentRole, SourceKind, Subagent, ToolCall, Usage,
    };

    fn fixed_now() -> i64 {
        // `Date.parse('2026-04-21T00:00:00.000Z')`
        parse_iso8601_ms("2026-04-21T00:00:00.000Z").expect("parse fixed_now")
    }

    fn tc(id: &str, name: &str, is_error: Option<bool>) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            target: None,
            args_hash: format!("{name}:{id}"),
            is_error,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    #[derive(Default, Clone)]
    struct TurnOverrides {
        message_id: String,
        turn_index: u64,
        ts: Option<String>,
        session_id: Option<String>,
        source: Option<SourceKind>,
        stop_reason: Option<Option<String>>,
        tool_calls: Option<Vec<ToolCall>>,
        retries: Option<Option<u64>>,
        has_edits: Option<bool>,
        subagent: Option<Subagent>,
    }

    fn turn(o: TurnOverrides) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: o.source.unwrap_or(SourceKind::ClaudeCode),
            session_id: o.session_id.unwrap_or_else(|| "s".to_string()),
            session_path: None,
            message_id: o.message_id,
            turn_index: o.turn_index,
            ts: o
                .ts
                .unwrap_or_else(|| "2026-04-20T00:00:00.000Z".to_string()),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 5,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: o.tool_calls.unwrap_or_default(),
            files_touched: None,
            subagent: o.subagent,
            stop_reason: o.stop_reason.unwrap_or_default(),
            activity: None,
            retries: match o.retries {
                Some(v) => v,
                None => Some(0),
            },
            has_edits: Some(o.has_edits.unwrap_or(false)),
            fidelity: None,
        }
    }

    #[test]
    fn empty_session_is_unknown_low() {
        let o = infer_outcome("s", &[], None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Unknown);
        assert_eq!(o.confidence, OutcomeConfidence::Low);
        assert_eq!(o.reason, OutcomeReason::Empty);
    }

    #[test]
    fn single_exchange_assistant_ended_is_completed_medium() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                stop_reason: Some(Some("tool_use".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
        ];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Completed);
        assert_eq!(o.confidence, OutcomeConfidence::Medium);
        assert_eq!(o.reason, OutcomeReason::SingleExchange);
    }

    #[test]
    fn one_turn_assistant_ended_is_single_exchange() {
        let turns = vec![turn(TurnOverrides {
            message_id: "m1".into(),
            turn_index: 0,
            stop_reason: Some(Some("end_turn".into())),
            ..Default::default()
        })];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Completed);
        assert_eq!(o.confidence, OutcomeConfidence::Medium);
        assert_eq!(o.reason, OutcomeReason::SingleExchange);
    }

    #[test]
    fn very_short_session_is_unknown_too_short() {
        let turns = vec![turn(TurnOverrides {
            message_id: "m1".into(),
            turn_index: 0,
            stop_reason: Some(Some("tool_use".into())),
            ..Default::default()
        })];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Unknown);
        assert_eq!(o.reason, OutcomeReason::TooShort);
    }

    #[test]
    fn recent_session_is_unknown_recent() {
        let now = parse_iso8601_ms("2026-04-20T00:05:00.000Z").unwrap();
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                ts: Some("2026-04-20T00:00:00.000Z".into()),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                ts: Some("2026-04-20T00:01:00.000Z".into()),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m3".into(),
                turn_index: 2,
                ts: Some("2026-04-20T00:02:00.000Z".into()),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
        ];
        let o = infer_outcome("s", &turns, None, now);
        assert_eq!(o.outcome, OutcomeLabel::Unknown);
        assert!(o.is_recent);
        assert_eq!(o.reason, OutcomeReason::Recent);
    }

    #[test]
    fn user_ended_long_is_abandoned_high() {
        let turns: Vec<TurnRecord> = (0..10)
            .map(|i| {
                turn(TurnOverrides {
                    message_id: format!("m{i}"),
                    turn_index: i as u64,
                    stop_reason: Some(Some(
                        if i == 9 { "tool_use" } else { "end_turn" }.to_string(),
                    )),
                    ..Default::default()
                })
            })
            .collect();
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Abandoned);
        assert_eq!(o.confidence, OutcomeConfidence::High);
        assert_eq!(o.reason, OutcomeReason::UserEndedLong);
    }

    #[test]
    fn user_ended_short_medium_is_abandoned_medium() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m3".into(),
                turn_index: 2,
                stop_reason: Some(Some("tool_use".into())),
                ..Default::default()
            }),
        ];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Abandoned);
        assert_eq!(o.confidence, OutcomeConfidence::Medium);
        assert_eq!(o.reason, OutcomeReason::UserEnded);
    }

    #[test]
    fn trailing_failure_streak_is_errored_medium() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                stop_reason: Some(Some("end_turn".into())),
                tool_calls: Some(vec![tc("u1", "Bash", Some(true))]),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m3".into(),
                turn_index: 2,
                stop_reason: Some(Some("end_turn".into())),
                tool_calls: Some(vec![tc("u2", "Bash", Some(true))]),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m4".into(),
                turn_index: 3,
                stop_reason: Some(Some("end_turn".into())),
                tool_calls: Some(vec![tc("u3", "Bash", Some(true))]),
                ..Default::default()
            }),
        ];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Errored);
        assert_eq!(o.reason, OutcomeReason::FailureStreak);
    }

    #[test]
    fn assistant_ended_is_completed_medium_default() {
        let turns: Vec<TurnRecord> = (0..3)
            .map(|i| {
                turn(TurnOverrides {
                    message_id: format!("m{}", i + 1),
                    turn_index: i,
                    stop_reason: Some(Some("end_turn".into())),
                    ..Default::default()
                })
            })
            .collect();
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Completed);
        assert_eq!(o.confidence, OutcomeConfidence::Medium);
        assert_eq!(o.reason, OutcomeReason::AssistantEnded);
    }

    #[test]
    fn no_stop_reason_is_completed_low_unknown_ending() {
        let turns: Vec<TurnRecord> = (0..3)
            .map(|i| {
                turn(TurnOverrides {
                    message_id: format!("m{}", i + 1),
                    turn_index: i,
                    source: Some(SourceKind::Codex),
                    stop_reason: Some(None),
                    ..Default::default()
                })
            })
            .collect();
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Completed);
        assert_eq!(o.confidence, OutcomeConfidence::Low);
        assert_eq!(o.reason, OutcomeReason::UnknownEnding);
    }

    #[test]
    fn no_stop_reason_still_detects_failure_streak() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                source: Some(SourceKind::Codex),
                stop_reason: Some(None),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                source: Some(SourceKind::Codex),
                stop_reason: Some(None),
                tool_calls: Some(vec![tc("u1", "Bash", Some(true))]),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m3".into(),
                turn_index: 2,
                source: Some(SourceKind::Codex),
                stop_reason: Some(None),
                tool_calls: Some(vec![tc("u2", "Bash", Some(true))]),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m4".into(),
                turn_index: 3,
                source: Some(SourceKind::Codex),
                stop_reason: Some(None),
                tool_calls: Some(vec![tc("u3", "Bash", Some(true))]),
                ..Default::default()
            }),
        ];
        let o = infer_outcome("s", &turns, None, fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Errored);
        assert_eq!(o.reason, OutcomeReason::FailureStreak);
    }

    #[test]
    fn give_up_phrase_downgrades_assistant_ended() {
        let turns: Vec<TurnRecord> = (0..3)
            .map(|i| {
                turn(TurnOverrides {
                    message_id: format!("m{}", i + 1),
                    turn_index: i,
                    stop_reason: Some(Some("end_turn".into())),
                    ..Default::default()
                })
            })
            .collect();
        let content = vec![ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "s".into(),
            message_id: "m3".into(),
            ts: "2026-04-20T00:00:00.000Z".into(),
            role: ContentRole::Assistant,
            kind: ContentKind::Text,
            text: Some("I'm unable to access the file, so I will stop here.".into()),
            tool_use: None,
            tool_result: None,
        }];
        let mut map: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        map.insert("s".to_string(), content);
        let o = infer_outcome("s", &turns, Some(&map), fixed_now());
        assert_eq!(o.outcome, OutcomeLabel::Completed);
        assert_eq!(o.confidence, OutcomeConfidence::Low);
        assert_eq!(o.reason, OutcomeReason::GiveUp);
    }

    #[test]
    fn one_shot_rate_counts_zero_retries_as_one_shot() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                has_edits: Some(true),
                retries: Some(Some(0)),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                has_edits: Some(true),
                retries: Some(Some(2)),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m3".into(),
                turn_index: 2,
                has_edits: Some(true),
                retries: Some(Some(0)),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m4".into(),
                turn_index: 3,
                has_edits: Some(false),
                retries: Some(Some(5)),
                ..Default::default()
            }),
        ];
        let m = compute_one_shot_rate("s", &turns);
        assert_eq!(m.edit_turns, 3);
        assert_eq!(m.one_shot_turns, 2);
        assert_eq!(m.one_shot_rate, Some(2.0 / 3.0));
        assert_eq!(m.total_retries, 2);
    }

    #[test]
    fn one_shot_rate_undefined_with_no_edit_turns() {
        let turns = vec![turn(TurnOverrides {
            message_id: "m1".into(),
            turn_index: 0,
            has_edits: Some(false),
            ..Default::default()
        })];
        let m = compute_one_shot_rate("s", &turns);
        assert_eq!(m.edit_turns, 0);
        assert_eq!(m.one_shot_rate, None);
    }

    #[test]
    fn one_shot_rate_excludes_sidechain_turns() {
        let sidechain = Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: None,
            parent_agent_id: None,
            subagent_type: None,
            description: None,
        };
        let turns = vec![
            turn(TurnOverrides {
                message_id: "m1".into(),
                turn_index: 0,
                has_edits: Some(true),
                retries: Some(Some(0)),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "m2".into(),
                turn_index: 1,
                has_edits: Some(true),
                retries: Some(Some(5)),
                subagent: Some(sidechain),
                ..Default::default()
            }),
        ];
        let m = compute_one_shot_rate("s", &turns);
        assert_eq!(m.edit_turns, 1);
        assert_eq!(m.one_shot_rate, Some(1.0));
    }

    #[test]
    fn compute_quality_pairs_outcome_and_one_shot_per_session() {
        let turns = vec![
            turn(TurnOverrides {
                message_id: "a1".into(),
                turn_index: 0,
                session_id: Some("A".into()),
                has_edits: Some(true),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "a2".into(),
                turn_index: 1,
                session_id: Some("A".into()),
                has_edits: Some(true),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "a3".into(),
                turn_index: 2,
                session_id: Some("A".into()),
                stop_reason: Some(Some("end_turn".into())),
                ..Default::default()
            }),
            turn(TurnOverrides {
                message_id: "b1".into(),
                turn_index: 0,
                session_id: Some("B".into()),
                stop_reason: Some(Some("tool_use".into())),
                ..Default::default()
            }),
        ];
        let q = compute_quality(
            &turns,
            &ComputeQualityOptions {
                content_by_session: None,
                now_ms: Some(fixed_now()),
            },
        );
        let a_out = q.outcomes.iter().find(|o| o.session_id == "A").unwrap();
        let b_out = q.outcomes.iter().find(|o| o.session_id == "B").unwrap();
        assert_eq!(a_out.outcome, OutcomeLabel::Completed);
        assert_eq!(b_out.outcome, OutcomeLabel::Unknown);
        assert_eq!(
            q.one_shot
                .iter()
                .find(|m| m.session_id == "A")
                .unwrap()
                .edit_turns,
            2
        );
        assert_eq!(
            q.one_shot
                .iter()
                .find(|m| m.session_id == "B")
                .unwrap()
                .edit_turns,
            0
        );
    }

    #[test]
    fn outcome_labels_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&OutcomeLabel::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeLabel::Abandoned).unwrap(),
            "\"abandoned\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeLabel::Errored).unwrap(),
            "\"errored\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeLabel::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeConfidence::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeReason::SingleExchange).unwrap(),
            "\"single-exchange\""
        );
        assert_eq!(
            serde_json::to_string(&OutcomeReason::UserEndedLong).unwrap(),
            "\"user-ended-long\""
        );
    }

    #[test]
    fn parse_iso_round_trip_to_known_epoch_ms() {
        // 2026-04-20T00:00:00.000Z
        let ms = parse_iso8601_ms("2026-04-20T00:00:00.000Z").unwrap();
        // (2026-1970)*365.25 days approximation; but we want exact equality
        // with chrono if available. Validate against another known anchor:
        let later = parse_iso8601_ms("2026-04-20T00:00:01.000Z").unwrap();
        assert_eq!(later - ms, 1000);
        let plus_minute = parse_iso8601_ms("2026-04-20T00:01:00.000Z").unwrap();
        assert_eq!(plus_minute - ms, 60_000);
        let plus_hour = parse_iso8601_ms("2026-04-20T01:00:00.000Z").unwrap();
        assert_eq!(plus_hour - ms, 3_600_000);
        let plus_day = parse_iso8601_ms("2026-04-21T00:00:00.000Z").unwrap();
        assert_eq!(plus_day - ms, 86_400_000);
    }

    #[test]
    fn parse_iso_rejects_out_of_range_components() {
        // Mirror `Date.parse` behavior: out-of-range minute / second / hour /
        // month / day all return NaN in JS, so the parser returns None here.
        assert!(parse_iso8601_ms("2026-04-20T23:60:00Z").is_none());
        assert!(parse_iso8601_ms("2026-04-20T23:59:60Z").is_none());
        assert!(parse_iso8601_ms("2026-04-20T24:00:00Z").is_none());
        assert!(parse_iso8601_ms("2026-13-20T00:00:00Z").is_none());
        assert!(parse_iso8601_ms("2026-04-32T00:00:00Z").is_none());
        assert!(parse_iso8601_ms("2026-00-20T00:00:00Z").is_none());
        assert!(parse_iso8601_ms("2026-04-00T00:00:00Z").is_none());
    }
}
