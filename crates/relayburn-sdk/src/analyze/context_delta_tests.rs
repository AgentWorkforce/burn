//! Conformance tests for the context_delta module — extracted verbatim from the
//! former inline `#[cfg(test)] mod tests` block (included via `#[path]`).

use super::*;
use crate::analyze::span_tree::{SpanKind, SpanNode, SpanStatus, TurnSpanTree};
use crate::reader::{CompactionEvent, SourceKind};

fn make_inf(req_id: &str, model: &str, input: i64, cache_read: i64, cache_write: i64) -> SpanNode {
    let mut n = SpanNode::new(SpanKind::Inference, model);
    n.set_attr("model", AttrValue::str(model));
    n.set_attr("request_id", AttrValue::str(req_id));
    n.set_attr("tokens.input", AttrValue::Int(input));
    n.set_attr("tokens.output", AttrValue::Int(0));
    n.set_attr("tokens.cache_read", AttrValue::Int(cache_read));
    n.set_attr("tokens.cache_write", AttrValue::Int(cache_write));
    n.set_attr("tokens.reasoning", AttrValue::Int(0));
    n
}

fn make_tool_use(name: &str, tool_use_id: &str) -> SpanNode {
    let mut n = SpanNode::new(SpanKind::ToolUse, name);
    n.set_attr("tool_use_id", AttrValue::str(tool_use_id));
    n
}

fn make_tool_result(tool_use_id: &str, bytes: i64, truncated: bool) -> SpanNode {
    let mut n = SpanNode::new(SpanKind::ToolResult, "tool-result");
    n.set_attr("tool_use_id", AttrValue::str(tool_use_id));
    n.set_attr("output_bytes", AttrValue::Int(bytes));
    if truncated {
        n.set_attr("output_truncated", AttrValue::Bool(true));
    }
    n
}

fn make_user_prompt() -> SpanNode {
    SpanNode::new(SpanKind::UserPrompt, "user-prompt")
}

fn turn_tree(session: &str, turn: &str, root: SpanNode) -> TurnSpanTree {
    TurnSpanTree {
        session_id: session.to_string(),
        turn_id: turn.to_string(),
        turn_number: 0,
        root,
    }
}

/// Build a single-turn fixture: two inferences on the main rail
/// with a Bash tool_result between them whose 40_000 bytes
/// translates to a ~10k token jump. The delta should surface as
/// the top row with Bash as the driver.
#[test]
fn bash_blowup_surfaces_as_top_delta_with_bash_driver() {
    // inference #1: context = 1000
    let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
    let mut bash_use = make_tool_use("Bash", "tu-1");
    bash_use
        .children
        .push(make_tool_result("tu-1", 40_000, false));
    let mut inf1 = inf1;
    inf1.children.push(bash_use);

    // inference #2: context jumped to 12_000 — delta of 11_000.
    let inf2 = make_inf("req-2", "claude-sonnet-4-6", 12_000, 0, 0);

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.status = SpanStatus::Ok;
    root.children.push(make_user_prompt());
    root.children.push(inf1);
    root.children.push(inf2);

    let tree = turn_tree("sess-1", "msg-1", root);
    let pricing = crate::analyze::pricing::load_builtin_pricing();
    let opts = ContextDeltaOpts::default();
    let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);
    assert_eq!(deltas.len(), 1, "one pairwise delta expected");
    let d = &deltas[0];
    assert_eq!(d.session_id, "sess-1");
    assert_eq!(d.turn_id, "msg-1");
    assert_eq!(d.owner_rail, OwnerRail::Main);
    assert_eq!(d.prior_context_tokens, 1000);
    assert_eq!(d.current_context_tokens, 12_000);
    assert_eq!(d.delta_tokens, 11_000);
    assert_eq!(d.intervening.len(), 1);
    match &d.intervening[0] {
        InterveningStep::ToolResult {
            tool_name,
            approx_bytes,
            approx_tokens,
            truncated,
            ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(*approx_bytes, 40_000);
            assert_eq!(*approx_tokens, 10_000);
            assert!(!*truncated);
        }
        other => panic!("expected ToolResult step, got {other:?}"),
    }
    // The driver_label helper should mention Bash.
    assert!(d.intervening[0].driver_label().contains("Bash"));
    // Cost is non-negative.
    assert!(d.attributed_cost_usd >= 0.0);
}

/// Compaction handling: a CompactionEvent between two inferences
/// where the second has *less* context than the first must surface
/// as `Compaction { tokens_freed }`, NOT a negative `delta_tokens`.
#[test]
fn compaction_replaces_negative_delta() {
    let inf1 = make_inf("req-1", "claude-sonnet-4-6", 50_000, 0, 0);
    // After compaction, context drops to 8_000.
    let inf2 = make_inf("req-2", "claude-sonnet-4-6", 8_000, 0, 0);

    // Stamp timestamps so the compaction event sits between them.
    let mut inf1 = inf1;
    inf1.start_ms = 1_776_643_201_000;
    inf1.end_ms = 1_776_643_202_000;
    let mut inf2 = inf2;
    inf2.start_ms = 1_776_643_204_000;
    inf2.end_ms = 1_776_643_205_000;

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children.push(inf1);
    root.children.push(inf2);
    let tree = turn_tree("sess-1", "msg-1", root);

    let compaction = CompactionEvent {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-1".into(),
        ts: "2026-04-20T00:00:03.000Z".into(),
        preceding_message_id: Some("msg-1".into()),
        tokens_before_compact: Some(50_000),
    };

    let pricing = crate::analyze::pricing::load_builtin_pricing();
    // Opts: min_delta 0 so the row isn't filtered out (delta_tokens is 0).
    let opts = ContextDeltaOpts {
        min_delta: Some(0),
        ..ContextDeltaOpts::default()
    };
    let deltas = deltas_for_session(&[tree], &[compaction], &pricing, &opts);
    assert_eq!(deltas.len(), 1);
    let d = &deltas[0];
    assert_eq!(d.delta_tokens, 0, "compaction clamps to 0");
    let has_compaction = d.intervening.iter().any(
        |s| matches!(s, InterveningStep::Compaction { tokens_freed } if *tokens_freed == 42_000),
    );
    assert!(
        has_compaction,
        "expected Compaction step with tokens_freed=42000, got {:?}",
        d.intervening
    );
}

/// Subagent isolation: a main-rail Inference and a subagent
/// Inference both happen, with a subagent tool_result between
/// them. The main-rail delta must NOT include the subagent's
/// tool_result.
#[test]
fn subagent_isolation_main_rail_excludes_subagent_results() {
    // Main rail: two inferences with a 10k context jump.
    let mut main_inf1 = make_inf("req-main-1", "claude-sonnet-4-6", 1000, 0, 0);
    // Add a Task tool_use that fans out to a Subagent with its own
    // inference + tool_result. The subagent's tool_result has
    // 40k bytes (~10k tokens) — and the main rail's tool_use also
    // gets a small result so the main delta is non-zero.
    let mut task_use = make_tool_use("Task", "tu-task");

    let mut sub_node = SpanNode::new(SpanKind::Subagent, "general-purpose");
    sub_node.set_attr("agent_id", AttrValue::str("agent-a"));
    // Subagent inferences:
    let sub_inf1 = make_inf("req-sub-1", "claude-sonnet-4-6", 2000, 0, 0);
    let mut sub_bash = make_tool_use("Bash", "tu-sub-bash");
    sub_bash
        .children
        .push(make_tool_result("tu-sub-bash", 40_000, false));
    let mut sub_inf1 = sub_inf1;
    sub_inf1.children.push(sub_bash);
    let sub_inf2 = make_inf("req-sub-2", "claude-sonnet-4-6", 12_000, 0, 0);
    sub_node.children.push(sub_inf1);
    sub_node.children.push(sub_inf2);
    task_use.children.push(sub_node);
    main_inf1.children.push(task_use);

    // Main rail #2: small jump only (no big tool_result on its own).
    let main_inf2 = make_inf("req-main-2", "claude-sonnet-4-6", 3000, 0, 0);

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children.push(main_inf1);
    root.children.push(main_inf2);
    let tree = turn_tree("sess-1", "msg-1", root);

    let pricing = crate::analyze::pricing::load_builtin_pricing();
    let opts = ContextDeltaOpts {
        min_delta: Some(0),
        ..ContextDeltaOpts::default()
    };
    let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);

    // We expect one main-rail delta and one subagent-rail delta.
    let main_delta = deltas
        .iter()
        .find(|d| d.owner_rail == OwnerRail::Main)
        .expect("main-rail delta missing");
    let sub_delta = deltas
        .iter()
        .find(
            |d| matches!(&d.owner_rail, OwnerRail::Subagent { agent_id } if agent_id == "agent-a"),
        )
        .expect("subagent-rail delta missing");

    // Main delta intervening must NOT include the subagent's tool_result.
    for step in &main_delta.intervening {
        if let InterveningStep::ToolResult { tool_use_id, .. } = step {
            assert_ne!(
                tool_use_id, "tu-sub-bash",
                "main rail must NOT see the subagent's tool_result"
            );
        }
    }

    // Subagent delta intervening SHOULD include its own Bash result.
    let sub_has_bash = sub_delta.intervening.iter().any(|s| match s {
        InterveningStep::ToolResult { tool_use_id, .. } => tool_use_id == "tu-sub-bash",
        _ => false,
    });
    assert!(
        sub_has_bash,
        "subagent rail must see its own Bash tool_result"
    );
}

/// Empty rail (single inference, no prev) → no delta emitted, no
/// panic.
#[test]
fn single_inference_yields_no_delta() {
    let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children.push(inf1);
    let tree = turn_tree("sess-1", "msg-1", root);
    let pricing = crate::analyze::pricing::load_builtin_pricing();
    let deltas = deltas_for_session(&[tree], &[], &pricing, &ContextDeltaOpts::default());
    assert!(
        deltas.is_empty(),
        "single inference must not emit a pairwise delta"
    );
}

/// `min_delta` filters out small jumps.
#[test]
fn min_delta_filters_small_jumps() {
    let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
    // Small jump: +500 tokens.
    let inf2 = make_inf("req-2", "claude-sonnet-4-6", 1500, 0, 0);

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children.push(inf1);
    root.children.push(inf2);
    let tree = turn_tree("sess-1", "msg-1", root);
    let pricing = crate::analyze::pricing::load_builtin_pricing();
    // Default min_delta is 1000; 500 < 1000 → filtered out.
    let deltas = deltas_for_session(&[tree], &[], &pricing, &ContextDeltaOpts::default());
    assert!(deltas.is_empty(), "500 token jump must be filtered");

    // Lower the threshold to 100 → row appears.
    let opts = ContextDeltaOpts {
        min_delta: Some(100),
        ..ContextDeltaOpts::default()
    };
    let deltas = deltas_for_session(
        &[turn_tree("sess-1", "msg-1", root_with_two_infs(1000, 1500))],
        &[],
        &pricing,
        &opts,
    );
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].delta_tokens, 500);
}

fn root_with_two_infs(ctx1: i64, ctx2: i64) -> SpanNode {
    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children
        .push(make_inf("req-1", "claude-sonnet-4-6", ctx1, 0, 0));
    root.children
        .push(make_inf("req-2", "claude-sonnet-4-6", ctx2, 0, 0));
    root
}

/// `--top N` caps output.
#[test]
fn top_caps_output() {
    // Build a tree with 5 inferences, each adding 5000 tokens.
    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    let ctx_steps = [1000, 6000, 11_000, 16_000, 21_000];
    for (i, c) in ctx_steps.iter().enumerate() {
        root.children
            .push(make_inf(&format!("req-{i}"), "claude-sonnet-4-6", *c, 0, 0));
    }
    let tree = turn_tree("sess-1", "msg-1", root);
    let pricing = crate::analyze::pricing::load_builtin_pricing();

    // No cap → 4 pairwise deltas (5 inferences = 4 windows).
    let opts = ContextDeltaOpts {
        min_delta: Some(0),
        ..ContextDeltaOpts::default()
    };
    let all = deltas_for_session(std::slice::from_ref(&tree), &[], &pricing, &opts);
    assert_eq!(all.len(), 4);

    // Cap at 2 → only the top 2 deltas.
    let opts = ContextDeltaOpts {
        min_delta: Some(0),
        top: Some(2),
        ..ContextDeltaOpts::default()
    };
    let top2 = deltas_for_session(&[tree], &[], &pricing, &opts);
    assert_eq!(top2.len(), 2);
}

/// `--owner main` filter excludes subagent rails.
#[test]
fn owner_filter_main_excludes_subagent_rail() {
    // Reuse the subagent-isolation fixture shape.
    let mut main_inf1 = make_inf("req-main-1", "claude-sonnet-4-6", 1000, 0, 0);
    let mut task_use = make_tool_use("Task", "tu-task");
    let mut sub_node = SpanNode::new(SpanKind::Subagent, "general-purpose");
    sub_node.set_attr("agent_id", AttrValue::str("agent-a"));
    sub_node
        .children
        .push(make_inf("req-sub-1", "claude-sonnet-4-6", 2000, 0, 0));
    sub_node
        .children
        .push(make_inf("req-sub-2", "claude-sonnet-4-6", 22_000, 0, 0));
    task_use.children.push(sub_node);
    main_inf1.children.push(task_use);
    let main_inf2 = make_inf("req-main-2", "claude-sonnet-4-6", 3000, 0, 0);

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    root.children.push(make_user_prompt());
    root.children.push(main_inf1);
    root.children.push(main_inf2);
    let tree = turn_tree("sess-1", "msg-1", root);

    let pricing = crate::analyze::pricing::load_builtin_pricing();
    let opts = ContextDeltaOpts {
        min_delta: Some(0),
        owner: OwnerFilter::Main,
        ..ContextDeltaOpts::default()
    };
    let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);
    for d in &deltas {
        assert_eq!(
            d.owner_rail,
            OwnerRail::Main,
            "owner filter Main must exclude subagent rails"
        );
    }
    assert!(!deltas.is_empty(), "expected at least one main-rail delta");
}

/// JSON shape: rail serializes with a `kind` discriminant, steps
/// keep their kebab-case kind tag. Catch wire-format drift early.
#[test]
fn json_shape_uses_kebab_case_discriminants() {
    let d = ContextDelta {
        session_id: "s".into(),
        turn_id: "t".into(),
        inference_idx: 2,
        owner_rail: OwnerRail::Subagent {
            agent_id: "agent-x".into(),
        },
        prior_context_tokens: 10,
        current_context_tokens: 20,
        delta_tokens: 10,
        intervening: vec![InterveningStep::ToolResult {
            tool_use_id: "tu-1".into(),
            tool_name: "Bash".into(),
            approx_tokens: 5,
            approx_bytes: 20,
            truncated: false,
        }],
        attributed_cost_usd: 0.0,
    };
    let s = serde_json::to_string(&d).unwrap();
    assert!(s.contains("\"kind\":\"subagent\""), "got {s}");
    assert!(s.contains("\"agentId\":\"agent-x\""), "got {s}");
    assert!(s.contains("\"kind\":\"tool-result\""), "got {s}");
    assert!(s.contains("\"toolUseId\":\"tu-1\""), "got {s}");
    let back: ContextDelta = serde_json::from_str(&s).unwrap();
    assert_eq!(back, d);
}

/// System reminder step surfaces as its own intervening step row.
/// We construct one directly (the timeline builder doesn't yet
/// synthesize SystemReminder spans from content sidecars; first-
/// cut behavior per the issue).
#[test]
fn system_reminder_step_round_trips_in_json() {
    let step = InterveningStep::SystemReminder {
        source: ReminderSource::Other,
        approx_tokens: 250,
    };
    let s = serde_json::to_string(&step).unwrap();
    assert!(s.contains("\"kind\":\"system-reminder\""), "got {s}");
    assert!(s.contains("\"source\":\"other\""), "got {s}");
    let back: InterveningStep = serde_json::from_str(&s).unwrap();
    assert_eq!(back, step);
}
