//! Conformance tests for the patterns module — Rust port of
//! `packages/analyze/src/patterns.test.ts`.
//!
//! Each TS `describe` block is preserved as one nested `mod` so the
//! one-to-one mapping between the two suites stays explicit.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::reader::{
    parse_claude_session, ClaudeParseOptions, CompactionEvent, ContentKind, ContentRecord,
    ContentRole, ContentToolResult, ContentToolUse, SourceKind, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, Usage, UserTurnBlock, UserTurnBlockKind,
    UserTurnRecord,
};
use serde_json::{json, Value};

use crate::analyze::patterns::{detect_patterns, DetectPatternsOptions};
use crate::analyze::pricing::load_builtin_pricing;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("claude")
        .join(name)
}

fn empty_usage() -> Usage {
    Usage {
        input: 10,
        output: 5,
        reasoning: 0,
        cache_read: 100,
        cache_create_5m: 50,
        cache_create_1h: 0,
    }
}

fn tc_(id: &str, name: &str, args_hash: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        target: None,
        args_hash: args_hash.into(),
        is_error: None,
        edit_pre_hash: None,
        edit_post_hash: None,
        skill_name: None,
        replaced_tools: None,
        collapsed_calls: None,
    }
}

fn tc_err(id: &str, name: &str, args_hash: &str) -> ToolCall {
    let mut c = tc_(id, name, args_hash);
    c.is_error = Some(true);
    c
}

fn tc_target(id: &str, name: &str, args_hash: &str, target: &str) -> ToolCall {
    let mut c = tc_(id, name, args_hash);
    c.target = Some(target.into());
    c
}

fn tc_edit(
    id: &str,
    name: &str,
    args_hash: &str,
    target: &str,
    pre: &str,
    post: &str,
) -> ToolCall {
    let mut c = tc_target(id, name, args_hash, target);
    c.edit_pre_hash = Some(pre.into());
    c.edit_post_hash = Some(post.into());
    c
}

fn tc_skill(id: &str, args_hash: &str, skill_name: &str) -> ToolCall {
    let mut c = tc_(id, "skill", args_hash);
    c.skill_name = Some(skill_name.into());
    c
}

fn turn(session_id: &str, message_id: &str, turn_index: u64) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session_id.into(),
        session_path: None,
        message_id: message_id.into(),
        turn_index,
        ts: "2026-04-20T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: None,
        project_key: None,
        usage: empty_usage(),
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

fn turn_with(
    session_id: &str,
    message_id: &str,
    turn_index: u64,
    source: SourceKind,
    usage: Usage,
    tool_calls: Vec<ToolCall>,
) -> TurnRecord {
    let mut t = turn(session_id, message_id, turn_index);
    t.source = source;
    t.usage = usage;
    t.tool_calls = tool_calls;
    t
}

fn evt(
    session_id: &str,
    tool_use_id: &str,
    event_index: u64,
    status: ToolResultStatus,
) -> ToolResultEventRecord {
    ToolResultEventRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session_id.into(),
        message_id: None,
        tool_use_id: tool_use_id.into(),
        call_index: None,
        event_index,
        ts: None,
        status,
        event_source: ToolResultEventSource::ToolResult,
        content_length: None,
        content_hash: None,
        is_error: None,
        usage: None,
        usage_attribution: None,
        subagent_session_id: None,
        agent_id: None,
        replaced_tools: None,
        collapsed_calls: None,
    }
}

fn evt_subagent(
    session_id: &str,
    tool_use_id: &str,
    event_index: u64,
) -> ToolResultEventRecord {
    let mut e = evt(session_id, tool_use_id, event_index, ToolResultStatus::Errored);
    e.event_source = ToolResultEventSource::SubagentNotification;
    e
}

fn user_turn_record(
    session_id: &str,
    user_uuid: &str,
    blocks: Vec<UserTurnBlock>,
) -> UserTurnRecord {
    UserTurnRecord {
        v: 1,
        source: SourceKind::Opencode,
        session_id: session_id.into(),
        user_uuid: user_uuid.into(),
        ts: "2026-04-20T00:00:00.000Z".into(),
        preceding_message_id: None,
        following_message_id: Some("m1".into()),
        blocks,
    }
}

fn text_block(byte_len: u64, approx_tokens: u64) -> UserTurnBlock {
    UserTurnBlock {
        kind: UserTurnBlockKind::Text,
        tool_use_id: None,
        byte_len,
        approx_tokens,
        is_error: None,
    }
}

fn tool_result_record(
    session_id: &str,
    message_id: &str,
    tool_use_id: &str,
    text: &str,
) -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session_id.into(),
        message_id: message_id.into(),
        ts: "2026-04-20T00:00:00.000Z".into(),
        role: ContentRole::ToolResult,
        kind: ContentKind::ToolResult,
        text: None,
        tool_use: None,
        tool_result: Some(ContentToolResult {
            tool_use_id: tool_use_id.into(),
            content: Value::String(text.into()),
            is_error: Some(true),
        }),
    }
}

fn tool_use_record(
    session_id: &str,
    message_id: &str,
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> ContentRecord {
    let map = match input {
        Value::Object(m) => m.into_iter().collect(),
        _ => Default::default(),
    };
    ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session_id.into(),
        message_id: message_id.into(),
        ts: "2026-04-20T00:00:00.000Z".into(),
        role: ContentRole::Assistant,
        kind: ContentKind::ToolUse,
        text: None,
        tool_use: Some(ContentToolUse {
            id: id.into(),
            name: name.into(),
            input: map,
        }),
        tool_result: None,
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — retry loops')
// ---------------------------------------------------------------------------

mod retry_loops {
    use super::*;

    #[test]
    fn reports_one_retry_loop_of_length_4_for_4_consecutive_identical_failing_bash_calls() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(fixture("retry-loop.jsonl"), &ClaudeParseOptions::default())
            .expect("parse retry-loop fixture");
        let result = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1);
        let loop_ = &result.retry_loops[0];
        assert_eq!(loop_.tool, "Bash");
        assert_eq!(loop_.attempts, 4);
        assert_eq!(loop_.start_turn_index, 0);
        assert_eq!(loop_.end_turn_index, 3);
        assert!(loop_.cost > 0.0, "cost should be nonzero");
    }

    #[test]
    fn reports_same_retry_loop_from_event_chronology_and_annotates_event_source() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(fixture("retry-loop.jsonl"), &ClaudeParseOptions::default())
            .expect("parse retry-loop fixture");
        let legacy = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        let graph = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                tool_result_events: Some(&res.tool_result_events),
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None
            },
        );
        assert_eq!(graph.retry_loops.len(), 1);
        assert_eq!(graph.failure_runs.len(), 0);
        let g = &graph.retry_loops[0];
        let l = &legacy.retry_loops[0];
        assert_eq!(
            g.event_source,
            Some(crate::analyze::findings::PatternEventSource::ToolResult)
        );
        assert_eq!(g.tool, l.tool);
        assert_eq!(g.attempts, l.attempts);
        assert_eq!(g.start_turn_index, l.start_turn_index);
        assert_eq!(g.end_turn_index, l.end_turn_index);
        assert_eq!(g.args_hash, l.args_hash);
        assert!((g.cost - l.cost).abs() < 1e-12, "cost matches legacy");
    }

    #[test]
    fn does_not_trigger_on_2_consecutive_failures() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_err("u1", "Bash", "abc"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_err("u2", "Bash", "abc"));
        let result = detect_patterns(
            &[t1, t2],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 0);
    }

    #[test]
    fn resets_streak_when_intervening_non_errored_call_breaks_it() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_err("u1", "Bash", "abc"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_err("u2", "Bash", "abc"));
        let mut t3 = turn("s", "m3", 2);
        t3.tool_calls.push(tc_("u3", "Bash", "abc"));
        let mut t4 = turn("s", "m4", 3);
        t4.tool_calls.push(tc_err("u4", "Bash", "abc"));
        let mut t5 = turn("s", "m5", 4);
        t5.tool_calls.push(tc_err("u5", "Bash", "abc"));
        let result = detect_patterns(
            &[t1, t2, t3, t4, t5],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 0);
    }

    #[test]
    fn mixes_graph_backed_with_legacy_fallback_sessions() {
        let pricing = load_builtin_pricing();
        let mut g1 = turn("graph", "g1", 0);
        g1.tool_calls.push(tc_("g1", "Bash", "same"));
        let mut g2 = turn("graph", "g2", 1);
        g2.tool_calls.push(tc_("g2", "Bash", "same"));
        let mut g3 = turn("graph", "g3", 2);
        g3.tool_calls.push(tc_("g3", "Bash", "same"));
        let mut f1 = turn("fallback", "f1", 0);
        f1.tool_calls.push(tc_err("f1", "Bash", "same"));
        let mut f2 = turn("fallback", "f2", 1);
        f2.tool_calls.push(tc_err("f2", "Bash", "same"));
        let mut f3 = turn("fallback", "f3", 2);
        f3.tool_calls.push(tc_err("f3", "Bash", "same"));
        let events = vec![
            evt("graph", "g1", 0, ToolResultStatus::Errored),
            evt("graph", "g2", 1, ToolResultStatus::Errored),
            evt("graph", "g3", 2, ToolResultStatus::Errored),
        ];
        let result = detect_patterns(
            &[g1, g2, g3, f1, f2, f3],
            &DetectPatternsOptions {
                pricing: &pricing,
                tool_result_events: Some(&events),
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None
            },
        );
        assert_eq!(result.retry_loops.len(), 2);
        let by_session: HashMap<&str, &crate::analyze::findings::RetryLoop> = result
            .retry_loops
            .iter()
            .map(|l| (l.session_id.as_str(), l))
            .collect();
        assert_eq!(
            by_session["graph"].event_source,
            Some(crate::analyze::findings::PatternEventSource::ToolResult)
        );
        assert_eq!(by_session["fallback"].event_source, None);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — consecutive failure runs')
// ---------------------------------------------------------------------------

mod consecutive_failure_runs {
    use super::*;

    #[test]
    fn reports_3_distinct_failing_tools_in_sequence_as_one_failure_run() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(
            fixture("consecutive-failures.jsonl"),
            &ClaudeParseOptions::default(),
        )
        .expect("parse consecutive-failures fixture");
        let result = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.failure_runs.len(), 1);
        let run = &result.failure_runs[0];
        assert_eq!(run.length, 3);
        let mut tools = run.tools_involved.clone();
        tools.sort();
        assert_eq!(tools, vec!["Bash", "Grep", "Read"]);
    }

    #[test]
    fn does_not_trigger_when_mixed_success_failure_breaks_streak() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_err("u1", "Bash", "a"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_("u2", "Read", "b"));
        let mut t3 = turn("s", "m3", 2);
        t3.tool_calls.push(tc_err("u3", "Grep", "c"));
        let mut t4 = turn("s", "m4", 3);
        t4.tool_calls.push(tc_err("u4", "Glob", "d"));
        let result = detect_patterns(
            &[t1, t2, t3, t4],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.failure_runs.len(), 0);
        assert_eq!(result.retry_loops.len(), 0);
    }

    #[test]
    fn does_not_double_report_a_retry_loop_as_a_failure_run() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(fixture("retry-loop.jsonl"), &ClaudeParseOptions::default())
            .expect("parse retry-loop fixture");
        let result = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1, "retry loop reported");
        assert_eq!(result.failure_runs.len(), 0, "same-key streak is NOT a failure run");
    }

    #[test]
    fn counts_chained_subagent_notification_errors() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_target("a1", "Agent", "agent:one", "agent-a"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_target("a2", "Agent", "agent:two", "agent-b"));
        let mut t3 = turn("s", "m3", 2);
        t3.tool_calls.push(tc_target("a3", "Agent", "agent:three", "agent-c"));
        let events = vec![
            evt_subagent("s", "a1", 0),
            evt_subagent("s", "a2", 1),
            evt_subagent("s", "a3", 2),
        ];
        let result = detect_patterns(
            &[t1, t2, t3],
            &DetectPatternsOptions {
                pricing: &pricing,
                tool_result_events: Some(&events),
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None
            },
        );
        assert_eq!(result.failure_runs.len(), 1);
        assert_eq!(result.failure_runs[0].length, 3);
        assert_eq!(
            result.failure_runs[0].event_source,
            Some(crate::analyze::findings::PatternEventSource::SubagentNotification)
        );
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — cancelled graph events')
// ---------------------------------------------------------------------------

mod cancelled_graph_events {
    use super::*;

    #[test]
    fn keeps_cancellations_out_of_retry_failure_detectors() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_err("c1", "Bash", "same"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_err("c2", "Bash", "same"));
        let mut t3 = turn("s", "m3", 2);
        t3.tool_calls.push(tc_err("c3", "Bash", "same"));
        let events = vec![
            evt("s", "c1", 0, ToolResultStatus::Cancelled),
            evt("s", "c2", 1, ToolResultStatus::Cancelled),
            evt("s", "c3", 2, ToolResultStatus::Cancelled),
        ];
        let result = detect_patterns(
            &[t1, t2, t3],
            &DetectPatternsOptions {
                pricing: &pricing,
                tool_result_events: Some(&events),
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None
            },
        );
        assert_eq!(result.retry_loops.len(), 0);
        assert_eq!(result.failure_runs.len(), 0);
        assert_eq!(result.cancelled_runs.len(), 1);
        assert_eq!(result.cancelled_runs[0].length, 3);
        assert_eq!(
            result.cancelled_runs[0].event_source,
            crate::analyze::findings::PatternEventSource::ToolResult
        );
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — compaction losses')
// ---------------------------------------------------------------------------

mod compaction_losses {
    use super::*;

    #[test]
    fn prices_compaction_against_preceding_turn_cache_read() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(
            fixture("compact-boundary.jsonl"),
            &ClaudeParseOptions::default(),
        )
        .expect("parse compact-boundary fixture");
        let result = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&res.events),
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.compactions.len(), 1);
        let c = &result.compactions[0];
        assert_eq!(c.tokens_before_compact, 9000);
        assert_eq!(c.preceding_message_id.as_deref(), Some("msg_c_1"));
        assert!(c.cache_lost_cost > 0.0);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — edit reverts')
// ---------------------------------------------------------------------------

mod edit_reverts {
    use super::*;

    #[test]
    fn detects_two_edit_cycle_where_b_reverts_a() {
        let pricing = load_builtin_pricing();
        let res = parse_claude_session(fixture("edit-revert.jsonl"), &ClaudeParseOptions::default())
            .expect("parse edit-revert fixture");
        let result = detect_patterns(
            &res.turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 1);
        let c = &result.edit_reverts[0];
        assert_eq!(c.file_path, "/src/foo.ts");
        assert_eq!(c.first_edit_turn_index, 0);
        assert!(c.revert_turn_index > c.first_edit_turn_index);
        assert_eq!(c.span_turns, c.revert_turn_index - c.first_edit_turn_index);
    }

    #[test]
    fn does_not_trigger_on_different_files() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls
            .push(tc_edit("u1", "Edit", "h1", "/a.ts", "hashA", "hashB"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls
            .push(tc_edit("u2", "Edit", "h2", "/b.ts", "hashB", "hashA"));
        let result = detect_patterns(
            &[t1, t2],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 0);
    }

    #[test]
    fn detects_a_to_c_reversion_through_b() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls
            .push(tc_edit("u1", "Edit", "h1", "/f.ts", "A", "B"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls
            .push(tc_edit("u2", "Edit", "h2", "/f.ts", "B", "C"));
        let mut t3 = turn("s", "m3", 2);
        t3.tool_calls
            .push(tc_edit("u3", "Edit", "h3", "/f.ts", "C", "A"));
        let result = detect_patterns(
            &[t1, t2, t3],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 1);
        let c = &result.edit_reverts[0];
        assert_eq!(c.first_edit_turn_index, 0);
        assert_eq!(c.revert_turn_index, 2);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — session summary rollup')
// ---------------------------------------------------------------------------

mod session_summary_rollup {
    use super::*;

    #[test]
    fn aggregates_counts_per_session() {
        let pricing = load_builtin_pricing();
        let retry =
            parse_claude_session(fixture("retry-loop.jsonl"), &ClaudeParseOptions::default())
                .expect("retry-loop");
        let revert =
            parse_claude_session(fixture("edit-revert.jsonl"), &ClaudeParseOptions::default())
                .expect("edit-revert");
        let compact = parse_claude_session(
            fixture("compact-boundary.jsonl"),
            &ClaudeParseOptions::default(),
        )
        .expect("compact-boundary");

        let mut all_turns = retry.turns.clone();
        all_turns.extend(revert.turns.clone());
        let result = detect_patterns(
            &all_turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&compact.events),
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );

        let retry_summary = result
            .session_summaries
            .iter()
            .find(|s| s.session_id == "retry-session")
            .expect("retry summary");
        assert_eq!(retry_summary.retry_loop_count, 1);
        assert_eq!(retry_summary.total_retries, 4);

        let revert_summary = result
            .session_summaries
            .iter()
            .find(|s| s.session_id == "revert-session")
            .expect("revert summary");
        assert_eq!(revert_summary.edit_revert_count, 1);

        let compact_summary = result
            .session_summaries
            .iter()
            .find(|s| s.session_id == "compact-session")
            .expect("compact-only session still appears in summary");
        assert_eq!(compact_summary.compaction_count, 1);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — defensive')
// ---------------------------------------------------------------------------

mod defensive {
    use super::*;

    #[test]
    fn returns_empty_results_when_no_turns_and_no_events() {
        let pricing = load_builtin_pricing();
        let no_compactions: Vec<CompactionEvent> = Vec::new();
        let result = detect_patterns(
            &[],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&no_compactions),
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert!(result.retry_loops.is_empty());
        assert!(result.failure_runs.is_empty());
        assert!(result.cancelled_runs.is_empty());
        assert!(result.compactions.is_empty());
        assert!(result.edit_reverts.is_empty());
        assert!(result.session_summaries.is_empty());
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — OpenCode skill recall duplicates')
// ---------------------------------------------------------------------------

mod opencode_skill_recall_dups {
    use super::*;

    fn opencode_turn_with_skill(message_id: &str, turn_index: u64, id: &str, skill: &str) -> TurnRecord {
        turn_with(
            "s",
            message_id,
            turn_index,
            SourceKind::Opencode,
            empty_usage(),
            vec![tc_skill(id, &format!("h{turn_index}"), skill)],
        )
    }

    #[test]
    fn detects_repeated_skill_calls_with_same_name_in_opencode() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            opencode_turn_with_skill("m1", 0, "u1", "react-component"),
            opencode_turn_with_skill("m2", 1, "u2", "react-component"),
            opencode_turn_with_skill("m3", 2, "u3", "react-component"),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_recall_dups.len(), 1);
        let dup = &result.skill_recall_dups[0];
        assert_eq!(dup.skill_name, "react-component");
        assert_eq!(dup.call_count, 3);
        assert_eq!(dup.first_turn_index, 0);
        assert_eq!(dup.last_turn_index, 2);
    }

    #[test]
    fn does_not_trigger_on_single_skill_call() {
        let pricing = load_builtin_pricing();
        let turns = vec![opencode_turn_with_skill("m1", 0, "u1", "react-component")];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_recall_dups.len(), 0);
    }

    #[test]
    fn does_not_trigger_for_non_opencode_sessions() {
        let pricing = load_builtin_pricing();
        let mut t1 = turn("s", "m1", 0);
        t1.tool_calls.push(tc_skill("u1", "h1", "react-component"));
        let mut t2 = turn("s", "m2", 1);
        t2.tool_calls.push(tc_skill("u2", "h2", "react-component"));
        let result = detect_patterns(
            &[t1, t2],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_recall_dups.len(), 0);
    }

    #[test]
    fn groups_different_skill_names_separately() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            opencode_turn_with_skill("m1", 0, "u1", "react-component"),
            opencode_turn_with_skill("m2", 1, "u2", "react-component"),
            opencode_turn_with_skill("m3", 2, "u3", "testing"),
            opencode_turn_with_skill("m4", 3, "u4", "testing"),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_recall_dups.len(), 2);
        let react = result
            .skill_recall_dups
            .iter()
            .find(|d| d.skill_name == "react-component")
            .expect("react-component dup");
        let testing = result
            .skill_recall_dups
            .iter()
            .find(|d| d.skill_name == "testing")
            .expect("testing dup");
        assert_eq!(react.call_count, 2);
        assert_eq!(testing.call_count, 2);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — OpenCode skill pruning protection')
// ---------------------------------------------------------------------------

mod opencode_skill_pruning_protection {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cache_create_5m: u64) -> Usage {
        Usage {
            input,
            output,
            reasoning: 0,
            cache_read,
            cache_create_5m,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn detects_skill_content_riding_in_cache_after_invocation() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn_with(
                "s",
                "m1",
                0,
                SourceKind::Opencode,
                usage(100, 50, 0, 200),
                vec![tc_skill("u1", "h1", "react-component")],
            ),
            turn_with(
                "s",
                "m2",
                1,
                SourceKind::Opencode,
                usage(50, 30, 300, 0),
                vec![],
            ),
            turn_with(
                "s",
                "m3",
                2,
                SourceKind::Opencode,
                usage(50, 30, 350, 0),
                vec![],
            ),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_pruning_protection.len(), 1);
        let ev = &result.skill_pruning_protection[0];
        assert_eq!(ev.skill_name, "react-component");
        assert_eq!(ev.invoked_turn_index, 0);
        assert_eq!(ev.riding_turns, 2);
        assert_eq!(ev.last_cached_turn_index, 2);
    }

    #[test]
    fn does_not_emit_when_no_subsequent_cache_read_turns() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn_with(
                "s",
                "m1",
                0,
                SourceKind::Opencode,
                usage(100, 50, 0, 200),
                vec![tc_skill("u1", "h1", "react-component")],
            ),
            turn_with(
                "s",
                "m2",
                1,
                SourceKind::Opencode,
                usage(50, 30, 0, 0),
                vec![],
            ),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_pruning_protection.len(), 0);
    }

    #[test]
    fn does_not_trigger_for_non_opencode_sessions() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn_with(
                "s",
                "m1",
                0,
                SourceKind::ClaudeCode,
                usage(100, 50, 0, 200),
                vec![tc_skill("u1", "h1", "react-component")],
            ),
            turn_with(
                "s",
                "m2",
                1,
                SourceKind::ClaudeCode,
                usage(50, 30, 300, 0),
                vec![],
            ),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.skill_pruning_protection.len(), 0);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — session summary includes skill detectors')
// ---------------------------------------------------------------------------

mod session_summary_includes_skill_detectors {
    use super::*;

    #[test]
    fn aggregates_skill_recall_dup_and_pruning_protection_counts() {
        let pricing = load_builtin_pricing();
        let usage_create = Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 200,
            cache_create_1h: 0,
        };
        let usage_read = Usage {
            input: 50,
            output: 30,
            reasoning: 0,
            cache_read: 300,
            cache_create_5m: 0,
            cache_create_1h: 0,
        };
        let turns = vec![
            turn_with(
                "s",
                "m1",
                0,
                SourceKind::Opencode,
                usage_create.clone(),
                vec![tc_skill("u1", "h1", "react-component")],
            ),
            turn_with(
                "s",
                "m2",
                1,
                SourceKind::Opencode,
                usage_create.clone(),
                vec![tc_skill("u2", "h2", "react-component")],
            ),
            turn_with(
                "s",
                "m3",
                2,
                SourceKind::Opencode,
                usage_read.clone(),
                vec![],
            ),
        ];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        let summary = result
            .session_summaries
            .iter()
            .find(|s| s.session_id == "s")
            .expect("summary present");
        assert_eq!(summary.skill_recall_dup_count, 1);
        // Both skill calls have a subsequent cacheRead turn, so each gets a
        // pruning entry.
        assert_eq!(summary.skill_pruning_protection_count, 2);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — OpenCode system prompt tax')
// ---------------------------------------------------------------------------

mod opencode_system_prompt_tax {
    use super::*;

    #[test]
    fn estimates_system_prompt_size_from_first_cache_create_minus_first_user_message() {
        let pricing = load_builtin_pricing();
        let t1_usage = Usage {
            input: 5000,
            output: 200,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 5200,
            cache_create_1h: 0,
        };
        let t2_usage = Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 5200,
            cache_create_5m: 0,
            cache_create_1h: 0,
        };
        let turns = vec![
            turn_with("s", "m1", 0, SourceKind::Opencode, t1_usage, vec![]),
            turn_with("s", "m2", 1, SourceKind::Opencode, t2_usage, vec![]),
        ];
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            "s".into(),
            vec![user_turn_record("s", "u1", vec![text_block(800, 200)])],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                user_turns_by_session: Some(&user_turns_by_session),
                compactions: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.system_prompt_taxes.len(), 1);
        let tax = &result.system_prompt_taxes[0];
        assert_eq!(tax.first_turn_cache_create, 5200);
        assert_eq!(tax.first_user_message_tokens, 200);
        assert_eq!(tax.estimated_system_prompt_tokens, 5000);
        assert_eq!(tax.riding_turns, 1);
    }

    #[test]
    fn does_not_emit_when_user_turn_data_unavailable() {
        let pricing = load_builtin_pricing();
        let usage = Usage {
            input: 5000,
            output: 200,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 5200,
            cache_create_1h: 0,
        };
        let turns = vec![turn_with("s", "m1", 0, SourceKind::Opencode, usage, vec![])];
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.system_prompt_taxes.len(), 0);
    }

    #[test]
    fn does_not_trigger_for_non_opencode_sessions() {
        let pricing = load_builtin_pricing();
        let usage = Usage {
            input: 5000,
            output: 200,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 5200,
            cache_create_1h: 0,
        };
        let turns = vec![turn_with("s", "m1", 0, SourceKind::ClaudeCode, usage, vec![])];
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            "s".into(),
            vec![user_turn_record("s", "u1", vec![text_block(800, 200)])],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                user_turns_by_session: Some(&user_turns_by_session),
                compactions: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.system_prompt_taxes.len(), 0);
    }

    #[test]
    fn excludes_first_turn_from_riding_turns_even_when_cache_read_nonzero() {
        let pricing = load_builtin_pricing();
        let t1 = Usage {
            input: 5000,
            output: 200,
            reasoning: 0,
            cache_read: 4000,
            cache_create_5m: 5200,
            cache_create_1h: 0,
        };
        let t2 = Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 5200,
            cache_create_5m: 0,
            cache_create_1h: 0,
        };
        let turns = vec![
            turn_with("s", "m1", 0, SourceKind::Opencode, t1, vec![]),
            turn_with("s", "m2", 1, SourceKind::Opencode, t2, vec![]),
        ];
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            "s".into(),
            vec![user_turn_record("s", "u1", vec![text_block(800, 200)])],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                user_turns_by_session: Some(&user_turns_by_session),
                compactions: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.system_prompt_taxes.len(), 1);
        assert_eq!(result.system_prompt_taxes[0].riding_turns, 1);
    }

    #[test]
    fn does_not_emit_when_first_cache_create_is_zero() {
        let pricing = load_builtin_pricing();
        let usage = Usage {
            input: 200,
            output: 100,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        };
        let turns = vec![turn_with("s", "m1", 0, SourceKind::Opencode, usage, vec![])];
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            "s".into(),
            vec![user_turn_record("s", "u1", vec![text_block(800, 200)])],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                user_turns_by_session: Some(&user_turns_by_session),
                compactions: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.system_prompt_taxes.len(), 0);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — edit-heavy sessions (cross-harness)')
// ---------------------------------------------------------------------------

mod edit_heavy_sessions {
    use super::*;

    fn edit_heavy_turns(source: SourceKind, edit_tool: &str, session_id: &str) -> Vec<TurnRecord> {
        (0..6_u64)
            .map(|i| {
                let mut t = turn(session_id, &format!("m{i}"), i);
                t.source = source;
                t.tool_calls.push(tc_target(
                    &format!("u{i}"),
                    edit_tool,
                    &format!("h{i}"),
                    &format!("/src/file{i}.ts"),
                ));
                t
            })
            .collect()
    }

    #[test]
    fn flags_claude_session_with_6_edits_and_0_reads() {
        let pricing = load_builtin_pricing();
        let turns = edit_heavy_turns(SourceKind::ClaudeCode, "Edit", "s");
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        let r = &result.edit_heavy_sessions[0];
        assert_eq!(r.source, SourceKind::ClaudeCode);
        assert_eq!(r.edit_count, 6);
        assert_eq!(r.read_count, 0);
        assert_eq!(r.ratio, f64::INFINITY);
    }

    #[test]
    fn flags_opencode_session_using_lowercase_tool_names() {
        let pricing = load_builtin_pricing();
        let turns = edit_heavy_turns(SourceKind::Opencode, "edit", "s");
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        assert_eq!(result.edit_heavy_sessions[0].source, SourceKind::Opencode);
        assert_eq!(result.edit_heavy_sessions[0].edit_count, 6);
    }

    #[test]
    fn flags_codex_session_using_apply_patch() {
        let pricing = load_builtin_pricing();
        let turns = edit_heavy_turns(SourceKind::Codex, "apply_patch", "s");
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        assert_eq!(result.edit_heavy_sessions[0].source, SourceKind::Codex);
        assert_eq!(result.edit_heavy_sessions[0].edit_count, 6);
    }

    #[test]
    fn does_not_flag_when_ratio_under_threshold() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::ClaudeCode, "Edit", "s");
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let mut t = turn("s", &format!("r{}", i + 1), 6 + i as u64);
            t.tool_calls.push(tc_target(
                &format!("r{}", i + 1),
                "Read",
                name,
                &format!("/{}.ts", name),
            ));
            turns.push(t);
        }
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 0);
    }

    #[test]
    fn does_not_flag_below_min_edits() {
        let pricing = load_builtin_pricing();
        let turns: Vec<TurnRecord> = (0..4_u64)
            .map(|i| {
                let mut t = turn("s", &format!("m{i}"), i);
                t.tool_calls.push(tc_target(
                    &format!("u{i}"),
                    "Edit",
                    &format!("h{i}"),
                    &format!("/f{i}.ts"),
                ));
                t
            })
            .collect();
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 0);
    }

    #[test]
    fn grep_glob_ls_bash_do_not_count_as_reads() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::ClaudeCode, "Edit", "s");
        for (i, (id, name, target)) in [
            ("g1", "Grep", None),
            ("g2", "Glob", None),
            ("g3", "LS", None),
            ("g4", "Bash", Some("cat /etc/hosts")),
        ]
        .iter()
        .enumerate()
        {
            let mut t = turn("s", id, 6 + i as u64);
            t.source = SourceKind::ClaudeCode;
            let mut call = tc_(id, name, &format!("h-{id}"));
            if let Some(tg) = target {
                call.target = Some((*tg).into());
            }
            t.tool_calls.push(call);
            turns.push(t);
        }
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        assert_eq!(result.edit_heavy_sessions[0].read_count, 0);
    }

    #[test]
    fn counts_codex_shell_cat_head_tail_file_reads() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::Codex, "apply_patch", "s");
        let mut r1 = turn("s", "r1", 6);
        r1.source = SourceKind::Codex;
        r1.tool_calls
            .push(tc_target("r1", "shell", "a", "cat package.json"));
        let mut r2 = turn("s", "r2", 7);
        r2.source = SourceKind::Codex;
        r2.tool_calls.push(tc_target(
            "r2",
            "exec_command",
            "b",
            "head -n 20 src/main.ts",
        ));
        turns.push(r1);
        turns.push(r2);
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 0);
    }

    #[test]
    fn reports_remaining_codex_edit_heavy_with_one_shell_file_read() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::Codex, "apply_patch", "s");
        let mut r1 = turn("s", "r1", 6);
        r1.source = SourceKind::Codex;
        r1.tool_calls.push(tc_target(
            "r1",
            "exec_command",
            "a",
            "tail -n 50 src/main.ts",
        ));
        turns.push(r1);
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        assert_eq!(result.edit_heavy_sessions[0].read_count, 1);
        assert_eq!(result.edit_heavy_sessions[0].ratio, 6.0);
    }

    #[test]
    fn does_not_count_unrelated_codex_shell_commands_as_reads() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::Codex, "apply_patch", "s");
        let mut b1 = turn("s", "b1", 6);
        b1.source = SourceKind::Codex;
        b1.tool_calls.push(tc_target("b1", "shell", "a", "git status"));
        let mut b2 = turn("s", "b2", 7);
        b2.source = SourceKind::Codex;
        b2.tool_calls.push(tc_target(
            "b2",
            "exec_command",
            "b",
            "git log --oneline | head -n 5",
        ));
        turns.push(b1);
        turns.push(b2);
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        assert_eq!(result.edit_heavy_sessions[0].read_count, 0);
    }

    #[test]
    fn codex_read_file_normalizes_to_read_and_brings_ratio_under_threshold() {
        let pricing = load_builtin_pricing();
        let mut turns = edit_heavy_turns(SourceKind::Codex, "apply_patch", "s");
        let mut r1 = turn("s", "r1", 6);
        r1.source = SourceKind::Codex;
        r1.tool_calls.push(tc_("r1", "read_file", "a"));
        let mut r2 = turn("s", "r2", 7);
        r2.source = SourceKind::Codex;
        r2.tool_calls.push(tc_("r2", "read_file", "b"));
        turns.push(r1);
        turns.push(r2);
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        // 6 edits + 2 reads → ratio 3.0, ≤ 4: no flag.
        assert_eq!(result.edit_heavy_sessions.len(), 0);
    }

    #[test]
    fn reports_likely_retries_from_intra_turn_edit_bash_edit() {
        let pricing = load_builtin_pricing();
        let turns: Vec<TurnRecord> = (0..5_u64)
            .map(|i| {
                let mut t = turn("s", &format!("m{i}"), i);
                t.tool_calls.push(tc_target(
                    &format!("e1-{i}"),
                    "Edit",
                    &format!("h{i}a"),
                    &format!("/f{i}.ts"),
                ));
                t.tool_calls.push(tc_target(
                    &format!("b-{i}"),
                    "Bash",
                    &format!("bh{i}"),
                    "pytest",
                ));
                t.tool_calls.push(tc_target(
                    &format!("e2-{i}"),
                    "Edit",
                    &format!("h{i}b"),
                    &format!("/f{i}.ts"),
                ));
                t
            })
            .collect();
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_heavy_sessions.len(), 1);
        let r = &result.edit_heavy_sessions[0];
        assert_eq!(r.edit_count, 10);
        assert_eq!(r.likely_retries, 5, "one retry per turn × 5 turns");
    }

    #[test]
    fn aggregates_edit_heavy_count_into_session_summary() {
        let pricing = load_builtin_pricing();
        let turns = edit_heavy_turns(SourceKind::ClaudeCode, "Edit", "s");
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        let summary = result
            .session_summaries
            .iter()
            .find(|s| s.session_id == "s")
            .expect("summary present");
        assert_eq!(summary.edit_heavy_count, 1);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — RetryLoop errorSignature enrichment (#57)')
// ---------------------------------------------------------------------------

mod retry_loop_error_signature {
    use super::*;

    fn errored_bash_turns(n: u64) -> Vec<TurnRecord> {
        (0..n)
            .map(|i| {
                let mut t = turn("s", &format!("m{i}"), i);
                t.tool_calls.push(tc_err(&format!("u{i}"), "Bash", "abc"));
                t
            })
            .collect()
    }

    #[test]
    fn populates_when_all_attempts_share_a_leading_line() {
        let pricing = load_builtin_pricing();
        let turns = errored_bash_turns(4);
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![
                tool_result_record("s", "m0", "u0", "npm ERR! code ENOENT\n  more details\n  more details"),
                tool_result_record("s", "m1", "u1", "npm ERR! code ENOENT\n  more details"),
                tool_result_record("s", "m2", "u2", "npm ERR! code ENOENT\n  trailing"),
                tool_result_record("s", "m3", "u3", "npm ERR! code ENOENT\n  yet again"),
            ],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1);
        assert_eq!(
            result.retry_loops[0].error_signature.as_deref(),
            Some("npm ERR! code ENOENT")
        );
    }

    #[test]
    fn marks_first_signature_with_diverged_when_attempts_differ() {
        let pricing = load_builtin_pricing();
        let turns = errored_bash_turns(3);
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![
                tool_result_record("s", "m0", "u0", "npm ERR! code ENOENT"),
                tool_result_record("s", "m1", "u1", "npm ERR! code EACCES"),
                tool_result_record("s", "m2", "u2", "npm ERR! code ENOENT"),
            ],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1);
        assert_eq!(
            result.retry_loops[0].error_signature.as_deref(),
            Some("npm ERR! code ENOENT (signatures diverged)")
        );
    }

    #[test]
    fn omits_when_no_content_by_session_supplied() {
        let pricing = load_builtin_pricing();
        let turns = errored_bash_turns(3);
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1);
        assert_eq!(result.retry_loops[0].error_signature, None);
    }

    #[test]
    fn omits_when_content_has_no_matching_tool_results() {
        let pricing = load_builtin_pricing();
        let turns = errored_bash_turns(3);
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![tool_result_record("s", "m99", "unrelated", "something else")],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.retry_loops.len(), 1);
        assert_eq!(result.retry_loops[0].error_signature, None);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — FailureRun errorSignatures enrichment (#57)')
// ---------------------------------------------------------------------------

mod failure_run_error_signatures {
    use super::*;

    fn three_distinct_failure_turns() -> Vec<TurnRecord> {
        let mut t0 = turn("s", "m0", 0);
        t0.tool_calls.push(tc_err("u0", "Bash", "a"));
        let mut t1 = turn("s", "m1", 1);
        t1.tool_calls.push(tc_err("u1", "Read", "b"));
        let mut t2 = turn("s", "m2", 2);
        t2.tool_calls.push(tc_err("u2", "Grep", "c"));
        vec![t0, t1, t2]
    }

    #[test]
    fn records_one_entry_per_distinct_tool_in_first_seen_order() {
        let pricing = load_builtin_pricing();
        let turns = three_distinct_failure_turns();
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![
                tool_result_record("s", "m0", "u0", "bash: command not found"),
                tool_result_record("s", "m1", "u1", "ENOENT: no such file or directory"),
                tool_result_record("s", "m2", "u2", "no matches found"),
            ],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.failure_runs.len(), 1);
        let sigs = result.failure_runs[0].error_signatures.as_ref().unwrap();
        assert_eq!(sigs.len(), 3);
        assert_eq!(sigs[0].tool, "Bash");
        assert_eq!(sigs[0].first_line, "bash: command not found");
        assert_eq!(sigs[1].tool, "Read");
        assert_eq!(sigs[1].first_line, "ENOENT: no such file or directory");
        assert_eq!(sigs[2].tool, "Grep");
        assert_eq!(sigs[2].first_line, "no matches found");
    }

    #[test]
    fn omits_when_content_not_supplied() {
        let pricing = load_builtin_pricing();
        let turns = three_distinct_failure_turns();
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.failure_runs.len(), 1);
        assert_eq!(result.failure_runs[0].error_signatures, None);
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — CompactionLoss lostWork enrichment (#57)')
// ---------------------------------------------------------------------------

mod compaction_loss_lost_work {
    use super::*;

    #[test]
    fn aggregates_files_and_tool_counts_in_compacted_window() {
        let pricing = load_builtin_pricing();
        let mut t0 = turn("s", "m0", 0);
        t0.ts = "2026-04-20T00:00:00.000Z".into();
        t0.tool_calls.push(tc_edit("u0", "Edit", "h0", "/src/foo.ts", "a", "b"));
        t0.tool_calls.push(tc_("u1", "Bash", "h1"));
        let mut t1 = turn("s", "m1", 1);
        t1.ts = "2026-04-20T00:00:01.000Z".into();
        t1.tool_calls.push(tc_edit("u2", "Edit", "h2", "/src/bar.ts", "c", "d"));
        t1.tool_calls.push(tc_("u3", "Read", "h3"));
        t1.tool_calls.push(tc_("u4", "Bash", "h4"));
        let events = vec![CompactionEvent {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "s".into(),
            ts: "2026-04-20T00:00:02.000Z".into(),
            preceding_message_id: Some("m1".into()),
            tokens_before_compact: Some(9000),
        }];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![tool_result_record("s", "m0", "u0", "present")],
        );
        let result = detect_patterns(
            &[t0, t1],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&events),
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.compactions.len(), 1);
        let lw = result.compactions[0].lost_work.as_ref().unwrap();
        assert_eq!(lw.files, vec!["/src/bar.ts", "/src/foo.ts"]);
        assert_eq!(lw.edit_count, 2);
        assert_eq!(lw.bash_count, 2);
        assert_eq!(lw.read_count, 1);
    }

    #[test]
    fn windows_successive_boundaries() {
        let pricing = load_builtin_pricing();
        let mut t0 = turn("s", "m0", 0);
        t0.ts = "2026-04-20T00:00:00.000Z".into();
        t0.tool_calls
            .push(tc_edit("u0", "Edit", "h0", "/a.ts", "a", "b"));
        let mut t1 = turn("s", "m1", 1);
        t1.ts = "2026-04-20T00:00:02.000Z".into();
        t1.tool_calls
            .push(tc_edit("u1", "Edit", "h1", "/b.ts", "c", "d"));
        let events = vec![
            CompactionEvent {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "s".into(),
                ts: "2026-04-20T00:00:01.000Z".into(),
                preceding_message_id: Some("m0".into()),
                tokens_before_compact: Some(5000),
            },
            CompactionEvent {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "s".into(),
                ts: "2026-04-20T00:00:03.000Z".into(),
                preceding_message_id: Some("m1".into()),
                tokens_before_compact: Some(7000),
            },
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![tool_result_record("s", "m0", "u0", "x")],
        );
        let result = detect_patterns(
            &[t0, t1],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&events),
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.compactions.len(), 2);
        assert_eq!(
            result.compactions[0].lost_work.as_ref().unwrap().files,
            vec!["/a.ts"]
        );
        assert_eq!(
            result.compactions[1].lost_work.as_ref().unwrap().files,
            vec!["/b.ts"]
        );
    }

    #[test]
    fn omits_lost_work_when_no_content_by_session_supplied() {
        let pricing = load_builtin_pricing();
        let mut t0 = turn("s", "m0", 0);
        t0.ts = "2026-04-20T00:00:00.000Z".into();
        t0.tool_calls
            .push(tc_edit("u0", "Edit", "h0", "/src/foo.ts", "a", "b"));
        let events = vec![CompactionEvent {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "s".into(),
            ts: "2026-04-20T00:00:01.000Z".into(),
            preceding_message_id: Some("m0".into()),
            tokens_before_compact: Some(1000),
        }];
        let result = detect_patterns(
            &[t0],
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: Some(&events),
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.compactions.len(), 1);
        assert!(result.compactions[0].lost_work.is_none());
    }
}

// ---------------------------------------------------------------------------
// describe('detectPatterns — EditRevertCycle samplePreview enrichment (#57)')
// ---------------------------------------------------------------------------

mod edit_revert_sample_preview {
    use super::*;

    fn ab_ba_turns() -> Vec<TurnRecord> {
        let mut t0 = turn("s", "m0", 0);
        t0.tool_calls
            .push(tc_edit("u0", "Edit", "h0", "/f.ts", "A", "B"));
        let mut t1 = turn("s", "m1", 1);
        t1.tool_calls
            .push(tc_edit("u1", "Edit", "h1", "/f.ts", "B", "A"));
        vec![t0, t1]
    }

    #[test]
    fn populates_with_truncated_old_new_strings_from_both_anchors() {
        let pricing = load_builtin_pricing();
        let turns = ab_ba_turns();
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![
                tool_use_record(
                    "s",
                    "m0",
                    "u0",
                    "Edit",
                    json!({"old_string": "foo", "new_string": "bar", "file_path": "/f.ts"}),
                ),
                tool_use_record(
                    "s",
                    "m1",
                    "u1",
                    "Edit",
                    json!({"old_string": "bar", "new_string": "foo", "file_path": "/f.ts"}),
                ),
            ],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 1);
        let preview = result.edit_reverts[0].sample_preview.as_ref().unwrap();
        assert_eq!(preview.first_edit.old, "foo");
        assert_eq!(preview.first_edit.new, "bar");
        assert_eq!(preview.revert.old, "bar");
        assert_eq!(preview.revert.new, "foo");
    }

    #[test]
    fn truncates_each_field_at_about_200_chars_with_ellipsis() {
        let pricing = load_builtin_pricing();
        let turns = ab_ba_turns();
        let long: String = std::iter::repeat_n('x', 500).collect();
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![
                tool_use_record(
                    "s",
                    "m0",
                    "u0",
                    "Edit",
                    json!({"old_string": long, "new_string": long}),
                ),
                tool_use_record(
                    "s",
                    "m1",
                    "u1",
                    "Edit",
                    json!({"old_string": long, "new_string": long}),
                ),
            ],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        let preview = result.edit_reverts[0].sample_preview.as_ref().unwrap();
        assert!(preview.first_edit.old.chars().count() <= 200);
        assert!(preview.first_edit.new.chars().count() <= 200);
        assert!(preview.first_edit.old.ends_with('…'));
    }

    #[test]
    fn omits_sample_preview_when_no_content_by_session_supplied() {
        let pricing = load_builtin_pricing();
        let turns = ab_ba_turns();
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                compactions: None,
                user_turns_by_session: None,
                content_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 1);
        assert!(result.edit_reverts[0].sample_preview.is_none());
    }

    #[test]
    fn omits_when_tool_use_entries_missing_from_content() {
        let pricing = load_builtin_pricing();
        let turns = ab_ba_turns();
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            "s".into(),
            vec![tool_result_record("s", "m0", "u0", "irrelevant")],
        );
        let result = detect_patterns(
            &turns,
            &DetectPatternsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                compactions: None,
                user_turns_by_session: None,
                tool_result_events: None
            },
        );
        assert_eq!(result.edit_reverts.len(), 1);
        assert!(result.edit_reverts[0].sample_preview.is_none());
    }
}
