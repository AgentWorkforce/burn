//! The reasoning section and its findings over constructed Codex turns.

use super::*;
use crate::analyze::load_pricing;
use crate::reader::{ReasoningConfig, Usage};

fn turn(index: u64, model: &str, effort: Option<&str>, activity: ActivityCategory) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: "sess".to_string(),
        session_path: None,
        message_id: format!("t{index}"),
        turn_index: index,
        ts: format!("2026-10-01T00:00:{index:02}.000Z"),
        model: model.to_string(),
        project: None,
        project_key: None,
        usage: Usage {
            input: 10_000,
            output: 3_000,
            reasoning: 2_000,
            ..Usage::default()
        },
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: Some(activity),
        retries: None,
        has_edits: None,
        fidelity: None,
        reasoning: effort.map(|effort| ReasoningConfig {
            effort: Some(effort.to_string()),
            summary: Some("detailed".to_string()),
        }),
    }
}

fn session(model: &str) -> Vec<TurnRecord> {
    vec![
        turn(0, model, Some("low"), ActivityCategory::Planning),
        turn(1, model, Some("high"), ActivityCategory::Git),
        turn(2, model, Some("high"), ActivityCategory::Exploration),
        turn(3, model, Some("high"), ActivityCategory::Git),
        turn(4, model, Some("medium"), ActivityCategory::Coding),
    ]
}

fn analyze(turns: &[TurnRecord]) -> (ReasoningBreakdown, Vec<Finding>) {
    let pricing = load_pricing(None);
    let report = breakdown(Harness::Codex, turns, &pricing)
        .data()
        .cloned()
        .expect("reasoning section");
    let cx = FindingContext::new(turns);
    let found = findings(turns, &report, &pricing, &cx);
    (report, found)
}

fn by_code<'a>(found: &'a [Finding], code: &str) -> Option<&'a Finding> {
    found.iter().find(|f| f.code == code)
}

#[test]
fn levels_sum_turns_tokens_and_cost_per_effort() {
    let (report, _) = analyze(&session("gpt-5.4"));
    let efforts: Vec<(Option<&str>, u64)> = report
        .levels
        .iter()
        .map(|row| (row.effort.as_deref(), row.turns))
        .collect();
    assert_eq!(
        efforts,
        [(Some("low"), 1), (Some("medium"), 1), (Some("high"), 3)]
    );
    let high = &report.levels[2];
    // Codex counts reasoning inside output: input + output per turn.
    assert_eq!(high.tokens, 3 * 13_000);
    assert_eq!(high.reasoning_tokens, 3 * 2_000);
    // gpt-5.4 output is $15/M: 6,000 reasoning tokens cost $0.09.
    assert!((high.reasoning_cost_usd.unwrap() - 0.09).abs() < 1e-9);
    assert!(high.cost_usd.unwrap() > high.reasoning_cost_usd.unwrap());
    let git = &high.activities[0];
    assert_eq!((git.category, git.turns), (Some(ActivityCategory::Git), 2));
}

#[test]
fn changes_are_recorded_between_turns_that_record_effort() {
    let mut turns = session("gpt-5.4");
    turns.insert(2, turn(9, "gpt-5.4", None, ActivityCategory::Git));
    let (report, _) = analyze(&turns);
    let changes: Vec<(u64, &str, &str)> = report
        .changes
        .iter()
        .map(|c| (c.turn_index, c.from.as_str(), c.to.as_str()))
        .collect();
    assert_eq!(changes, [(1, "low", "high"), (4, "high", "medium")]);
    assert_eq!(report.levels.last().unwrap().effort, None);
}

#[test]
fn deep_effort_on_routine_work_is_a_finding_with_its_reasoning_spend() {
    let (_, found) = analyze(&session("gpt-5.4"));
    let routine = by_code(&found, "high-effort-routine-work").expect("routine finding");
    assert_eq!(routine.evidence.turn_indexes, [1, 2, 3]);
    assert_eq!(routine.evidence.targets, ["git", "exploration"]);
    assert_eq!(routine.impact.tokens, Some(6_000));
    assert!((routine.impact.cost_usd.unwrap() - 0.09).abs() < 1e-9);
    assert!(
        routine
            .explanation
            .contains("3 of 5 turns ran at high effort"),
        "{}",
        routine.explanation
    );
}

#[test]
fn effort_changes_explain_per_turn_cost_at_each_level() {
    let (_, found) = analyze(&session("gpt-5.4"));
    let change = by_code(&found, "reasoning-effort-change").expect("change finding");
    assert_eq!(change.evidence.turn_indexes, [1, 4]);
    assert!(
        change
            .explanation
            .contains("Effort went from low to high at turn 1, high to medium at turn 4."),
        "{}",
        change.explanation
    );
    assert!(
        change
            .explanation
            .contains("high averaged 13,000 tokens (2,000 reasoning, $"),
        "{}",
        change.explanation
    );
    assert_eq!(change.impact.tokens, None);
}

#[test]
fn routine_work_below_the_thresholds_is_not_a_finding() {
    let mut turns = session("gpt-5.4");
    turns[3].reasoning = None;
    let (_, found) = analyze(&turns);
    assert!(by_code(&found, "high-effort-routine-work").is_none());

    let mut turns = session("gpt-5.4");
    for turn in &mut turns {
        turn.usage.reasoning = 0;
    }
    let (_, found) = analyze(&turns);
    assert!(by_code(&found, "high-effort-routine-work").is_none());

    // Routine turns carrying under a quarter of the session's spend.
    let mut turns = session("gpt-5.4");
    turns[0].usage.input = 1_000_000;
    let (_, found) = analyze(&turns);
    assert!(by_code(&found, "high-effort-routine-work").is_none());
}

#[test]
fn unpriced_models_report_unknown_cost_never_zero() {
    let (report, found) = analyze(&session("unpriced-house-model"));
    assert!(report
        .levels
        .iter()
        .all(|row| row.cost_usd.is_none() && row.reasoning_cost_usd.is_none()));
    let routine = by_code(&found, "high-effort-routine-work").expect("ranked by tokens");
    assert_eq!(routine.impact.cost_usd, None);
    assert_eq!(routine.impact.pricing, FindingPricingStatus::Unpriced);
    assert_eq!(routine.severity, WasteSeverity::Info);
    assert!(routine.explanation.contains("session's tokens"));
    assert!(
        !routine.explanation.contains('$'),
        "{}",
        routine.explanation
    );
    let change = by_code(&found, "reasoning-effort-change").unwrap();
    assert_eq!(change.impact.pricing, FindingPricingStatus::Unpriced);
    assert!(!change.explanation.contains('$'), "{}", change.explanation);
}

#[test]
fn sessions_without_recorded_effort_say_why() {
    let pricing = load_pricing(None);
    let mut turns = session("gpt-5.4");
    for turn in &mut turns {
        turn.reasoning = None;
    }
    assert_eq!(
        breakdown(Harness::Codex, &turns, &pricing).reason(),
        Some("no turn of this codex session records a reasoning effort")
    );
    assert_eq!(
        breakdown(Harness::Codex, &[], &pricing).reason(),
        Some("the session has no assistant turns")
    );
}

#[test]
fn unknown_effort_names_sort_after_known_levels() {
    assert!(order(Some("high")) < order(Some("turbo")));
    assert!(order(Some("turbo")) < order(None));
    assert!(is_deep("xhigh") && is_deep("high") && !is_deep("medium") && !is_deep("turbo"));
}

#[test]
fn counts_group_thousands() {
    assert_eq!(count(0), "0");
    assert_eq!(count(999), "999");
    assert_eq!(count(1_000), "1,000");
    assert_eq!(count(1_234_567), "1,234,567");
}
