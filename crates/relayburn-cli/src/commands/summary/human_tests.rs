use super::*;
use relayburn_sdk::{OneShotMetrics, SessionOutcome};

fn outcome(label: OutcomeLabel) -> SessionOutcome {
    let mut o: SessionOutcome = serde_json::from_value(json!({
        "sessionId": "s",
        "outcome": "unknown",
        "confidence": "high",
        "isRecent": false,
        "reason": "user-ended"
    }))
    .expect("valid SessionOutcome fixture");
    o.outcome = label;
    o
}

fn one_shot(edit_turns: u64, one_shot_turns: u64) -> OneShotMetrics {
    OneShotMetrics {
        session_id: "s".into(),
        edit_turns,
        one_shot_turns,
        one_shot_rate: None,
        total_retries: 0,
    }
}

#[test]
fn render_quality_without_sessions() {
    assert_eq!(
        render_quality(&QualityResult::default()),
        "quality: (no sessions)"
    );
}

#[test]
fn render_quality_counts_each_outcome_and_rate() {
    let mut outcomes = vec![outcome(OutcomeLabel::Completed); 1_001];
    outcomes.extend(vec![outcome(OutcomeLabel::Abandoned); 2]);
    outcomes.extend(vec![outcome(OutcomeLabel::Errored); 3]);
    outcomes.extend(vec![outcome(OutcomeLabel::Unknown); 4]);
    let q = QualityResult {
        outcomes,
        one_shot: vec![one_shot(1_000, 500), one_shot(200, 100), one_shot(0, 0)],
    };
    assert_eq!(
        render_quality(&q),
        "quality — sessions: 1,010\n  \
         outcomes: 1,001 completed / 2 abandoned / 3 errored / 4 unknown\n  \
         one-shot rate: 50.0% across 1,200 edit turns"
    );
}

#[test]
fn render_quality_rate_uses_summed_turns() {
    let q = QualityResult {
        outcomes: vec![outcome(OutcomeLabel::Errored)],
        one_shot: vec![one_shot(3, 1), one_shot(0, 0)],
    };
    assert_eq!(
        render_quality(&q),
        "quality — sessions: 1\n  \
         outcomes: 0 completed / 0 abandoned / 1 errored / 0 unknown\n  \
         one-shot rate: 33.3% across 3 edit turns"
    );
}

#[test]
fn render_quality_without_edit_turns() {
    let q = QualityResult {
        outcomes: vec![
            outcome(OutcomeLabel::Abandoned),
            outcome(OutcomeLabel::Unknown),
        ],
        one_shot: vec![one_shot(0, 0)],
    };
    assert_eq!(
        render_quality(&q),
        "quality — sessions: 2\n  \
         outcomes: 0 completed / 1 abandoned / 0 errored / 1 unknown\n  \
         one-shot rate: n/a (no edit turns)"
    );
}
