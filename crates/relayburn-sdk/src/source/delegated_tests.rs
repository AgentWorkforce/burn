//! Delegated subagents billed with the session that spawned them.

use std::path::Path;

use ai_hist::{ProviderRoots, SessionIdentity, SessionRef, SessionStore, StoreOptions};

use super::*;
use crate::analyze::pricing::load_builtin_pricing;
use crate::query_verbs::subagent_tree_for_session;
use crate::reader::Harness;
use crate::session_metrics::turn_total_tokens;
use crate::source::locate::{discover, read_evidence};
use crate::source::snapshot_tests::fixtures_root;
use crate::source::stage::stage_path;
use crate::{analyze_session, AnalyzeSessionOptions, SessionLocator};

/// The root evidence of a staged transcript and its delegated children.
fn load(harness: Harness, path: &Path) -> (SessionEvidence, Vec<SessionEvidence>) {
    let staged = stage_path(harness, path).unwrap();
    let store = staged.open_store().unwrap();
    let reference = staged.hydrate(&store).unwrap();
    let root = read_evidence(&store, &reference).unwrap();
    let children = delegated_children(&store, &root).unwrap();
    (root, children)
}

fn sidecar_session() -> (SessionEvidence, Vec<SessionEvidence>) {
    load(
        Harness::ClaudeCode,
        &fixtures_root().join("claude-sidecars/sidecar-session.jsonl"),
    )
}

#[test]
fn subagent_turns_are_billed_once_as_turns_of_the_root_session() {
    let (root, children) = sidecar_session();
    let ids: Vec<&str> = children
        .iter()
        .map(|c| c.session.session_id.as_str())
        .collect();
    assert_eq!(ids, ["a1", "a2"]);
    let records = records_with_children(&root, &children);
    let turns: Vec<(&str, u64, Option<&str>, Option<&str>)> = records
        .turns
        .iter()
        .map(|t| {
            let sub = t.subagent.as_ref();
            (
                t.message_id.as_str(),
                t.turn_index,
                sub.and_then(|s| s.agent_id.as_deref()),
                sub.and_then(|s| s.parent_agent_id.as_deref()),
            )
        })
        .collect();
    assert_eq!(
        turns,
        [
            ("msg_main_1", 0, None, None),
            ("msg_main_2", 1, None, None),
            ("msg_sub1_1", 2, Some("a1"), Some("sidecar-session")),
            ("msg_sub1_2", 3, Some("a1"), Some("sidecar-session")),
            ("msg_sub2_1", 4, Some("a2"), Some("a1")),
        ]
    );
    assert!(records
        .turns
        .iter()
        .all(|t| t.session_id == "sidecar-session"));
    let reviewer = records.turns[4].subagent.as_ref().unwrap();
    assert_eq!(reviewer.parent_tool_use_id.as_deref(), Some("toolu_review"));
    assert_eq!(reviewer.subagent_type.as_deref(), Some("code-reviewer"));
    assert_eq!(reviewer.description.as_deref(), Some("Review the map"));
    assert!(records
        .content
        .iter()
        .all(|c| c.session_id == "sidecar-session"));
    assert!(records
        .user_turns
        .iter()
        .all(|u| u.session_id == "sidecar-session"));
    assert_eq!(records.request_id_lookup.len(), 5);
}

#[test]
fn a_grandchild_nests_under_the_subagent_that_spawned_it() {
    let (root, children) = sidecar_session();
    let records = records_with_children(&root, &children);
    let edges: Vec<(&str, Option<&str>, Option<&str>)> = records
        .relationships
        .iter()
        .filter(|r| r.relationship_type == RelationshipType::Subagent)
        .map(|r| {
            (
                r.session_id.as_str(),
                r.related_session_id.as_deref(),
                r.agent_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        [
            ("sidecar-session", Some("sidecar-session"), Some("a1")),
            ("sidecar-session", Some("a1"), Some("a2")),
        ]
    );

    let pricing = load_builtin_pricing();
    let tree = subagent_tree_for_session(
        &records.turns,
        &records.relationships,
        &pricing,
        "sidecar-session",
    )
    .unwrap();
    let total: u64 = records.turns.iter().map(turn_total_tokens).sum();
    assert_eq!(tree.cumulative_turns, 5);
    assert_eq!(tree.cumulative_tokens, total);
    assert_eq!(tree.self_tokens, 120 + 260);
    let explore = &tree.children[0];
    assert_eq!(explore.node_id, "a1");
    assert_eq!(explore.self_turns, 2);
    assert_eq!(explore.self_tokens, 330 + 440);
    let reviewer = &explore.children[0];
    assert_eq!(reviewer.node_id, "a2");
    assert_eq!(reviewer.label, "code-reviewer");
    assert_eq!(reviewer.cumulative_tokens, 225);
    let costs = [tree.self_cost, explore.self_cost, reviewer.self_cost];
    let sum: f64 = costs.iter().map(|c| c.expect("priced")).sum();
    assert!((tree.cumulative_cost.unwrap() - sum).abs() < 1e-12);
}

#[test]
fn analyze_bills_subagents_and_pairs_them_into_the_flow() {
    let analysis = analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::ClaudeCode,
        path: fixtures_root().join("claude-sidecars/sidecar-session.jsonl"),
    }))
    .unwrap();
    let metrics = analysis.metrics.data().unwrap();
    assert_eq!(metrics.turn_count, 5);
    assert_eq!(metrics.usage.total_tokens, 120 + 260 + 330 + 440 + 225);
    let flow = analysis.flow.data().unwrap();
    assert_eq!((flow.turns, flow.subagents), (2, 2));
    let tree = analysis.subagents.data().unwrap();
    assert_eq!(tree.children[0].children[0].node_id, "a2");
}

#[test]
fn a_session_without_subagents_is_unchanged() {
    let (root, children) = load(
        Harness::ClaudeCode,
        &fixtures_root().join("claude/simple-turn.jsonl"),
    );
    assert!(children.is_empty());
    let value = |r: &SessionRecords| serde_json::to_value(r).unwrap();
    assert_eq!(
        value(&records_with_children(&root, &children)),
        value(&records_from_evidence(&root))
    );
}

/// A Codex install holding a parent rollout and the subagent thread it
/// spawned.
fn codex_install(home: &Path) -> SessionStore {
    let roots = ProviderRoots::from_home(
        home.to_path_buf(),
        home.join(".local/share/opencode/opencode.db"),
    );
    let day = roots.codex.join("sessions/2026/04/23");
    std::fs::create_dir_all(&day).unwrap();
    for (fixture, name) in [
        ("with-spawn-agent.jsonl", "rollout-parent.jsonl"),
        (
            "../codex-delegated/subagent-child.jsonl",
            "rollout-child.jsonl",
        ),
    ] {
        std::fs::copy(fixtures_root().join("codex").join(fixture), day.join(name)).unwrap();
    }
    let mut options = StoreOptions::default();
    options.db_path = Some(home.join("ai-history.db"));
    options.roots = Some(roots);
    SessionStore::open(options).unwrap()
}

#[test]
fn a_codex_child_thread_is_not_folded_into_its_parent() {
    let home = tempfile::tempdir().unwrap();
    let store = codex_install(home.path());
    discover(&store, Source::Codex).unwrap();
    let parent = SessionRef::id(Source::Codex, "sess_spawn_1");
    store.hydrate(&parent, Default::default()).unwrap();
    let root = read_evidence(&store, &parent).unwrap();
    // relayhistory reaches the child through delegation; burn bills it as
    // its own session.
    let reached = store
        .delegated_descendants(&[SessionIdentity::new("codex", "sess_spawn_1")])
        .unwrap();
    assert_eq!(reached, [SessionIdentity::new("codex", "agent_inv_42")]);
    let children = delegated_children(&store, &root).unwrap();
    assert!(children.is_empty());
    let records = records_with_children(&root, &children);
    assert_eq!(records.turns.len(), 1);
    assert!(records.turns.iter().all(|t| t.session_id == "sess_spawn_1"));
}

#[test]
fn a_codex_child_rollout_analyzes_by_path_as_its_own_session() {
    let analysis = analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::Codex,
        path: fixtures_root().join("codex-delegated/subagent-child.jsonl"),
    }))
    .unwrap();
    assert_eq!(analysis.session.session_id, "agent_inv_42");
    assert_eq!(analysis.metrics.data().unwrap().turn_count, 1);
}
