//! Sidechain derivations over evidence that carries the sidechain's user
//! records. relayhistory does not capture those records yet, so the parity
//! corpus cannot exercise the invocation walk; these tests restore them on
//! the staged fixture's evidence and expect the builtin reader's snapshot.

use ai_hist::{
    CatalogQuery, ProviderRoots, SessionEvidence, SessionQuery, SessionStore, StoreOptions,
};
use serde_json::{json, Value};

use crate::source::relayhistory::records_from_evidence;
use crate::source::snapshot_tests::{fixtures_root, render_value, snapshot_dir};

/// The fixture's evidence as JSON, synced through a throwaway HOME.
fn evidence_json(fixture: &str) -> (tempfile::TempDir, Value) {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude/projects/-tmp-project");
    std::fs::create_dir_all(&dir).unwrap();
    let file = format!("{fixture}.jsonl");
    std::fs::copy(fixtures_root().join("claude").join(&file), dir.join(&file)).unwrap();
    let mut options = StoreOptions::default();
    options.db_path = Some(home.path().join("ai-history.db"));
    options.roots = Some(ProviderRoots::from_home(
        home.path().to_path_buf(),
        home.path().join("opencode.db"),
    ));
    let store = SessionStore::open(options).unwrap();
    store.sync(Default::default()).unwrap();
    let row = store
        .sessions(CatalogQuery::default())
        .next()
        .unwrap()
        .unwrap();
    let ev = store
        .session(&row.session_ref(), SessionQuery::default())
        .unwrap()
        .unwrap();
    (home, serde_json::to_value(ev).unwrap())
}

fn sidechain_user(id: &str, parent: &str, ts_ms: i64, block: Value) -> Value {
    json!({
        "message_id": id, "role": block["role"], "ts_ms": ts_ms, "parent_id": parent,
        "is_sidechain": true, "cwd": "/tmp/project",
        "blocks": [block],
    })
}

fn text_block(id: &str, text: &str, ts_ms: i64) -> Value {
    json!({
        "event_uid": format!("{id}:0"), "role": "user", "kind": "text",
        "text": text, "text_bytes": text.len(), "ts_ms": ts_ms,
    })
}

fn assert_matches_snapshot(name: &str, ev: Value, home: &tempfile::TempDir) {
    let ev: SessionEvidence = serde_json::from_value(ev).unwrap();
    let got = render_value(&records_from_evidence(&ev), home.path());
    let path = snapshot_dir().join(format!("claude-{name}.json"));
    let want: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(got, want);
}

#[test]
fn nested_sidechains_resolve_through_their_invocation_chain() {
    let (home, mut ev) = evidence_json("nested-subagent");
    let base = 1_776_643_200_000_i64;
    let result = json!({
        "event_uid": "u-sub1-toolresult:0", "role": "tool_result", "kind": "tool_result",
        "text": "looks good", "text_bytes": 10, "tool_use_id": "toolu_inner",
        "raw_kind": "tool_result_block", "ts_ms": base + 6000,
    });
    let messages = ev["messages"].as_array_mut().unwrap();
    messages.push(sidechain_user(
        "u-sub1-user",
        "u-main-asst",
        base + 2000,
        text_block("u-sub1-user", "Research", base + 2000),
    ));
    messages.push(sidechain_user(
        "u-sub2-user",
        "u-sub1-asst",
        base + 4000,
        text_block("u-sub2-user", "Review the code", base + 4000),
    ));
    messages.push(sidechain_user(
        "u-sub1-toolresult",
        "u-sub1-asst",
        base + 6000,
        result,
    ));
    messages.sort_by_key(|m| m["ts_ms"].as_i64());
    let results = ev["tool_results"].as_array_mut().unwrap();
    results[0]["event_index"] = json!(1);
    results.insert(
        0,
        json!({
            "event_uid": "u-sub1-toolresult:0", "message_id": "u-sub1-toolresult",
            "ts_ms": base + 6000, "tool_use_id": "toolu_inner", "call_index": 0,
            "event_index": 0, "payload_bytes": 10, "payload_truncated": false,
            "payload_hash": "cada083c27dc6cd8", "result_status": "completed",
            "event_source": "tool_result", "text": "looks good", "text_bytes": 10,
        }),
    );
    assert_matches_snapshot("nested-subagent", ev, &home);
}

#[test]
fn a_leading_sidechain_prompt_is_a_user_turn_without_a_following_turn() {
    let (home, mut ev) = evidence_json("sidechain-leading-then-main");
    let ts = 1_776_816_000_000_i64;
    let messages = ev["messages"].as_array_mut().unwrap();
    messages.insert(
        0,
        sidechain_user(
            "u-side-1",
            "u-spawn",
            ts,
            text_block("u-side-1", "sidechain prompt", ts),
        ),
    );
    assert_matches_snapshot("sidechain-leading-then-main", ev, &home);
}
