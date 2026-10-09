//! Subagent sidecars (`<sessionId>/subagents/agent-<agentId>.jsonl` plus
//! `.meta.json`) the main session spawned become subagent relationships of
//! it. A subagent a subagent spawned is delegated by that subagent, so its
//! edge comes from the spawner's evidence (see `source::delegated`).

use serde_json::{json, Value};

use crate::reader::Harness;
use crate::source::locate::{load_session, SessionLocator};
use crate::source::relayhistory::records_from_evidence;
use crate::source::snapshot_tests::fixtures_root;

#[test]
fn sidecars_the_main_session_spawned_are_its_subagent_relationships() {
    let path = fixtures_root().join("claude-sidecars/sidecar-session.jsonl");
    let loaded = load_session(
        &SessionLocator::Path {
            harness: Harness::ClaudeCode,
            path,
        },
        &Default::default(),
    )
    .unwrap();
    let records = records_from_evidence(&loaded.evidence);
    let subagents: Vec<Value> = records
        .relationships
        .iter()
        .skip(1)
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    let edge = |agent: &str, ts: &str, call: &str, kind: &str, description: &str| {
        json!({
            "v": 1, "source": "native-claude", "sessionId": "sidecar-session",
            "relatedSessionId": "sidecar-session", "relationshipType": "subagent",
            "ts": ts, "sourceVersion": "2.1.120", "parentToolUseId": call,
            "agentId": agent, "subagentType": kind, "description": description,
        })
    };
    assert_eq!(
        subagents,
        vec![edge(
            "a1",
            "2026-07-01T00:00:02.000Z",
            "toolu_explore",
            "Explore",
            "Map the repo"
        )]
    );
}
