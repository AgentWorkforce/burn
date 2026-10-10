use super::*;

use crate::analyze::findings::WasteSeverity;
use crate::analyze::pricing::{ModelCost, ReasoningMode};
use crate::reader::{SourceKind, Subagent, UserTurnBlock};

const MODEL: &str = "cached-model";
const T0: &str = "2026-04-20T00:00:00.000Z";

fn pricing() -> PricingTable {
    let mut p = PricingTable::new();
    p.insert(
        MODEL.into(),
        ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            cache_write_1h: 6.0,
            reasoning: None,
            reasoning_mode: ReasoningMode::SameAsOutput,
            context_tiers: Vec::new(),
        },
    );
    p
}

fn usage(cache_read: u64, create_5m: u64, create_1h: u64) -> Usage {
    Usage {
        input: 10,
        output: 100,
        cache_read,
        cache_create_5m: create_5m,
        cache_create_1h: create_1h,
        ..Usage::default()
    }
}

/// ISO timestamp `minutes` after [`T0`].
fn at(minutes: i64) -> String {
    format!("2026-04-20T{:02}:{:02}:00.000Z", minutes / 60, minutes % 60)
}

fn turn(message_id: &str, minutes: i64, usage: Usage) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "s".into(),
        session_path: None,
        message_id: message_id.into(),
        turn_index: 0,
        ts: at(minutes),
        model: MODEL.into(),
        project: None,
        project_key: None,
        usage,
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

fn subagent_turn(message_id: &str, minutes: i64, usage: Usage) -> TurnRecord {
    TurnRecord {
        subagent: Some(Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: Some("agent-1".into()),
            parent_agent_id: None,
            subagent_type: None,
            description: None,
        }),
        ..turn(message_id, minutes, usage)
    }
}

fn user_turn(following: &str, kind: UserTurnBlockKind) -> UserTurnRecord {
    UserTurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "s".into(),
        user_uuid: format!("u-{following}"),
        ts: T0.into(),
        preceding_message_id: None,
        following_message_id: Some(following.into()),
        blocks: vec![UserTurnBlock {
            kind,
            tool_use_id: None,
            byte_len: 10,
            approx_tokens: 3,
            is_error: None,
        }],
    }
}

fn detect(turns: &[TurnRecord]) -> Vec<CacheExpiry> {
    detect_cache_expiry(turns, &[], &pricing(), None)
}

fn single_event(turns: &[TurnRecord]) -> CacheExpiryEvent {
    let found = detect(turns);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].events.len(), 1, "{found:?}");
    found[0].events[0].clone()
}

/// Warm 100k-token context written with the 1-hour TTL.
fn warm_1h(message_id: &str, minutes: i64) -> TurnRecord {
    turn(message_id, minutes, usage(90_000, 0, 9_990))
}

#[test]
fn prices_cold_recreation_after_the_1h_ttl() {
    let event = single_event(&[warm_1h("a", 0), turn("b", 120, usage(0, 0, 101_000))]);
    assert_eq!(event.message_id, "b");
    assert_eq!(event.gap_ms, 120 * 60_000);
    assert_eq!(event.ttl, CacheTtl::OneHour);
    assert_eq!(event.cause, ExpiryCause::Unknown);
    assert_eq!(event.recreated_tokens, 100_000);
    // 100k of the 101k written at $6/M, minus 100k warm reads at $0.30/M.
    let expected = 101_000.0 * 6.0 / 1e6 * (100_000.0 / 101_000.0) - 100_000.0 * 0.3 / 1e6;
    assert!((event.penalty_usd - expected).abs() < 1e-12);
}

#[test]
fn gap_must_exceed_the_ttl() {
    assert!(detect(&[warm_1h("a", 0), turn("b", 60, usage(0, 0, 101_000))]).is_empty());
    assert_eq!(
        single_event(&[warm_1h("a", 0), turn("b", 61, usage(0, 0, 101_000))]).ttl,
        CacheTtl::OneHour
    );
}

#[test]
fn five_minute_writes_expire_after_five_minutes() {
    let warm_5m = turn("a", 0, usage(90_000, 9_990, 0));
    assert!(detect(&[warm_5m.clone(), turn("b", 5, usage(0, 100_000, 0))]).is_empty());
    let event = single_event(&[warm_5m, turn("b", 6, usage(0, 100_000, 0))]);
    assert_eq!(event.ttl, CacheTtl::FiveMinutes);
    let expected = 100_000.0 * (3.75 - 0.3) / 1e6;
    assert!((event.penalty_usd - expected).abs() < 1e-12);
}

#[test]
fn read_only_turns_keep_the_last_written_ttl() {
    let turns = [
        warm_1h("a", 0),
        turn("b", 1, usage(100_000, 0, 0)),
        turn("c", 40, usage(0, 0, 101_000)),
    ];
    assert!(detect(&turns).is_empty());
}

#[test]
fn defaults_to_five_minute_ttl_before_any_cache_write() {
    let turns = [
        turn("a", 0, usage(100_000, 0, 0)),
        turn("b", 6, usage(0, 0, 100_000)),
    ];
    assert_eq!(single_event(&turns).ttl, CacheTtl::FiveMinutes);
}

#[test]
fn counts_only_the_previous_context_left_unread() {
    // 100k previous context; reading all but 1,023 tokens is a warm cache.
    let turns = [warm_1h("a", 0), turn("b", 120, usage(98_977, 0, 2_000))];
    assert!(detect(&turns).is_empty());
    let event = single_event(&[warm_1h("a", 0), turn("b", 120, usage(98_976, 0, 2_000))]);
    assert_eq!(event.recreated_tokens, 1_024);
}

#[test]
fn mixed_ttl_writes_expire_on_the_five_minute_portion() {
    // 20k 1-hour prefix stays warm; the 80k 5-minute suffix expires.
    let mixed = turn("a", 0, usage(0, 80_000, 20_000));
    assert!(detect(&[mixed.clone(), turn("b", 5, usage(20_000, 80_000, 0))]).is_empty());
    let event = single_event(&[mixed, turn("b", 10, usage(20_000, 80_000, 0))]);
    assert_eq!(event.ttl, CacheTtl::FiveMinutes);
    assert_eq!(event.recreated_tokens, 80_000);
}

#[test]
fn compacted_context_is_not_expiry() {
    // A context below half the previous one was compacted, not re-created.
    let shrunk = Usage {
        input: 0,
        ..usage(0, 0, 49_999)
    };
    assert!(detect(&[warm_1h("a", 0), turn("b", 120, shrunk)]).is_empty());
    let half = Usage {
        input: 0,
        ..usage(0, 0, 50_000)
    };
    assert_eq!(
        single_event(&[warm_1h("a", 0), turn("b", 120, half)]).recreated_tokens,
        50_000
    );
}

#[test]
fn each_model_keeps_its_own_cache() {
    let mut other_model = turn("b", 5, usage(0, 0, 101_000));
    other_model.model = "other-model".into();
    let mut synthetic = turn("s", 6, Usage::default());
    synthetic.model = "<synthetic>".into();
    // The other model's cold write is not this model's expiry; returning to
    // this model is measured against its own last turn.
    let event = single_event(&[
        warm_1h("a", 0),
        other_model,
        synthetic,
        turn("c", 120, usage(0, 0, 101_000)),
    ]);
    assert_eq!(event.message_id, "c");
    assert_eq!(event.gap_ms, 120 * 60_000);
}

#[test]
fn ignores_recreations_below_the_minimum_cacheable_prefix() {
    let small_prev = turn("a", 0, usage(1_000, 0, 1_014));
    assert!(detect(&[small_prev.clone(), turn("b", 120, usage(0, 0, 1_023))]).is_empty());
    assert_eq!(
        single_event(&[small_prev, turn("b", 120, usage(0, 0, 1_024))]).recreated_tokens,
        1_024
    );
}

#[test]
fn unpriced_models_are_skipped() {
    let turns = [warm_1h("a", 0), turn("b", 120, usage(0, 0, 101_000))];
    assert!(detect_cache_expiry(&turns, &[], &PricingTable::new(), None).is_empty());
}

#[test]
fn subagent_turns_do_not_share_the_main_thread_cache() {
    let turns = [
        turn("main-1", 0, usage(90_000, 9_990, 0)),
        subagent_turn("sub-1", 1, usage(0, 2_000, 0)),
        subagent_turn("sub-2", 2, usage(2_000, 100, 0)),
        turn("main-2", 20, usage(0, 100_000, 0)),
    ];
    let event = single_event(&turns);
    assert_eq!(event.message_id, "main-2");
    assert_eq!(event.recreated_tokens, 100_000);
}

fn unidentified_sidechain_turn(message_id: &str, minutes: i64, usage: Usage) -> TurnRecord {
    let mut t = subagent_turn(message_id, minutes, usage);
    t.subagent.as_mut().unwrap().agent_id = None;
    t
}

#[test]
fn unidentified_sidechains_beside_a_main_thread_are_skipped() {
    let turns = [
        turn("main-1", 0, usage(90_000, 9_990, 0)),
        unidentified_sidechain_turn("side-1", 3, usage(0, 2_000, 0)),
        unidentified_sidechain_turn("side-2", 40, usage(0, 50_000, 0)),
        turn("main-2", 20, usage(0, 100_000, 0)),
    ];
    let event = single_event(&turns);
    assert_eq!(event.message_id, "main-2");
    assert_eq!(event.recreated_tokens, 100_000);
}

#[test]
fn an_unidentified_sidechain_session_is_its_own_cache() {
    let mut turns = [
        unidentified_sidechain_turn("child-1", 0, usage(90_000, 9_990, 0)),
        unidentified_sidechain_turn("child-2", 20, usage(0, 100_000, 0)),
    ];
    for t in &mut turns {
        t.session_id = "child".into();
    }
    assert_eq!(single_event(&turns).message_id, "child-2");
}

#[test]
fn report_only_limits_events_to_the_listed_turns() {
    let turns = [
        warm_1h("a", 0),
        turn("b", 120, usage(0, 0, 101_000)),
        turn("c", 300, usage(0, 0, 101_100)),
    ];
    let reported = |ids: &[&str]| -> Vec<String> {
        let ids: HashSet<TurnId<'_>> = ids.iter().map(|id| ("s", *id)).collect();
        detect_cache_expiry(&turns, &[], &pricing(), Some(&ids))
            .into_iter()
            .flat_map(|e| e.events)
            .map(|e| e.message_id)
            .collect()
    };
    assert_eq!(reported(&["b", "c"]), ["b", "c"]);
    assert_eq!(reported(&["c"]), ["c"]);
    assert!(reported(&["a"]).is_empty());
}

#[test]
fn cache_state_turns_keep_the_latest_turn_and_latest_write_per_cache() {
    let mut other_model = turn("x", 20, usage(0, 0, 5_000));
    other_model.model = "other-model".into();
    let history = [
        turn("b", 10, usage(100_000, 0, 0)),
        warm_1h("a", 0),
        turn("old", 0, usage(0, 0, 0)),
        other_model,
    ];
    let mut kept: Vec<String> = cache_state_turns(&history)
        .into_iter()
        .map(|t| t.message_id)
        .collect();
    kept.sort();
    assert_eq!(kept, ["a", "b", "x"]);

    // b only read the cache; a's 1-hour write still sets the TTL, so a
    // resume 30 minutes after b is warm.
    let mut window = cache_state_turns(&history);
    window.push(turn("c", 40, usage(0, 0, 101_000)));
    let report: HashSet<TurnId<'_>> = HashSet::from([("s", "c")]);
    assert!(detect_cache_expiry(&window, &[], &pricing(), Some(&report)).is_empty());
}

#[test]
fn orders_turns_by_timestamp_and_drops_unparseable_ones() {
    let mut bad_ts = turn("x", 0, usage(0, 0, 0));
    bad_ts.ts = "not-a-timestamp".into();
    let turns = [
        turn("b", 120, usage(0, 0, 101_000)),
        bad_ts,
        warm_1h("a", 0),
    ];
    assert_eq!(single_event(&turns).message_id, "b");
}

#[test]
fn groups_events_per_session_and_skips_quiet_sessions() {
    let mut quiet = warm_1h("q", 0);
    quiet.session_id = "quiet".into();
    let turns = [
        warm_1h("a", 0),
        turn("b", 120, usage(0, 0, 101_000)),
        turn("c", 300, usage(0, 0, 101_100)),
        quiet,
    ];
    let found = detect(&turns);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].session_id, "s");
    assert_eq!(found[0].events.len(), 2);
}

#[test]
fn classifies_cause_from_the_preceding_user_turn() {
    let turns = [
        warm_1h("a", 0),
        turn("b", 120, usage(0, 0, 101_000)),
        turn("c", 300, usage(0, 0, 101_100)),
    ];
    let user_turns = [
        user_turn("b", UserTurnBlockKind::Text),
        user_turn("c", UserTurnBlockKind::ToolResult),
    ];
    let found = detect_cache_expiry(&turns, &user_turns, &pricing(), None);
    let causes: Vec<ExpiryCause> = found[0].events.iter().map(|e| e.cause).collect();
    assert_eq!(causes, [ExpiryCause::UserIdle, ExpiryCause::ToolWait]);
}

fn event(cause: ExpiryCause, gap_minutes: i64, penalty_usd: f64) -> CacheExpiryEvent {
    CacheExpiryEvent {
        message_id: "m".into(),
        gap_ms: gap_minutes * 60_000,
        ttl: CacheTtl::OneHour,
        cause,
        recreated_tokens: 100_000,
        penalty_usd,
    }
}

fn paste_text(finding: &WasteFinding) -> &str {
    match finding.actions.last() {
        Some(WasteAction::Paste { text, .. }) => text,
        other => panic!("expected a paste action, got {other:?}"),
    }
}

#[test]
fn finding_summarizes_the_session() {
    let expiry = CacheExpiry {
        session_id: "s".into(),
        events: vec![
            event(ExpiryCause::UserIdle, 190, 0.5),
            event(ExpiryCause::ToolWait, 70, 0.25),
        ],
    };
    let finding = cache_expiry_to_finding(&expiry);
    assert_eq!(finding.kind, "cache-expiry");
    assert_eq!(finding.session_id, "s");
    assert_eq!(finding.severity, WasteSeverity::High);
    assert_eq!(
        finding.title,
        "Prompt cache expired 2× before resuming (longest gap 3h10m)"
    );
    assert_eq!(
        finding.detail,
        "200,000 tokens of context were re-written to the prompt cache after its TTL lapsed \
(longest gap 3h10m against a 1-hour TTL), costing $0.7500 more than warm cache reads."
    );
    assert_eq!(finding.estimated_savings.usd_per_session, Some(0.75));
    assert_eq!(finding.estimated_savings.tokens_per_session, Some(200_000));
    assert_eq!(finding.actions.len(), 2);
    assert!(paste_text(&finding).contains("/compact"));
}

#[test]
fn advice_follows_the_costliest_cause() {
    let tool_heavy = CacheExpiry {
        session_id: "s".into(),
        events: vec![
            event(ExpiryCause::UserIdle, 70, 0.1),
            event(ExpiryCause::ToolWait, 70, 0.15),
            event(ExpiryCause::ToolWait, 70, 0.15),
            event(ExpiryCause::Unknown, 70, 0.25),
        ],
    };
    assert_eq!(dominant_cause(&tool_heavy), ExpiryCause::ToolWait);
    assert!(paste_text(&cache_expiry_to_finding(&tool_heavy)).contains("background"));
}

#[test]
fn formats_gaps() {
    assert_eq!(format_gap(45 * 60_000), "45m");
    assert_eq!(format_gap(120 * 60_000), "2h");
    assert_eq!(format_gap(190 * 60_000 + 59_999), "3h10m");
    assert_eq!(format_gap(65 * 60_000), "1h05m");
}
