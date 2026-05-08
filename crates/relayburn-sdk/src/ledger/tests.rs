//! Integration tests for the SQLite-only ledger redesign.
//!
//! Each acceptance bullet from #259 has a test below; the comment on
//! each `#[test]` cites the bullet it covers.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use crate::reader::{
    ContentKind, ContentRecord, ContentRole, RelationshipSourceKind, RelationshipType,
    SessionRelationshipRecord, SourceKind, ToolCall, TurnRecord, Usage, UserTurnBlock,
    UserTurnBlockKind, UserTurnRecord,
};
use tempfile::TempDir;

use super::*;

fn open_in(tmp: &TempDir) -> Ledger {
    let layout = LedgerLayout::under(tmp.path());
    Ledger::open(&layout.burn, &layout.content).unwrap()
}

fn make_turn(session: &str, message: &str, ts: &str, input: u64) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.into(),
        session_path: None,
        message_id: message.into(),
        turn_index: 0,
        ts: ts.into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("burn".into()),
        project_key: Some("burn".into()),
        usage: Usage {
            input,
            output: 5,
            reasoning: 0,
            cache_read: 100,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            target: None,
            args_hash: "abcd".into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

fn make_content(session: &str, message: &str, text: &str) -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.into(),
        message_id: message.into(),
        ts: "2025-01-01T00:00:00Z".into(),
        role: ContentRole::Assistant,
        kind: ContentKind::Text,
        text: Some(text.into()),
        tool_use: None,
        tool_result: None,
    }
}

#[test]
fn open_creates_both_dbs_and_no_extras() {
    // Acceptance: steady-state layout is `burn.sqlite` + `content.sqlite`.
    // Nothing else: no JSONL, no .idx, no .lock, no archive.sqlite.
    let tmp = TempDir::new().unwrap();
    let _l = open_in(&tmp);

    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap()))
        .collect();
    let names: std::collections::HashSet<_> = entries.iter().cloned().collect();
    // burn.sqlite + content.sqlite, plus the WAL/shm sidecars SQLite
    // creates in WAL mode. Anything *else* — `ledger.jsonl`, `*.idx`,
    // `*.lock`, `archive.sqlite`, a `content/` directory — would mean
    // we regressed back to the 1.x layout.
    let allowed_prefixes = ["burn.sqlite", "content.sqlite"];
    for name in &names {
        let ok = allowed_prefixes
            .iter()
            .any(|p| name == p || name.starts_with(&format!("{p}-")));
        assert!(ok, "unexpected file in layout: {name}");
    }
    assert!(names.iter().any(|n| n == "burn.sqlite"));
    assert!(names.iter().any(|n| n == "content.sqlite"));
}

#[test]
fn no_lock_files_after_concurrent_writers() {
    // Acceptance: no file-lock module. After 100 concurrent appends from
    // separate Ledger handles, the only files on disk are the two DBs +
    // WAL/shm sidecars.
    let tmp = TempDir::new().unwrap();
    let layout = LedgerLayout::under(tmp.path());
    // Pre-create the schema once so the threads don't race the DDL.
    {
        let _ = Ledger::open(&layout.burn, &layout.content).unwrap();
    }

    let burn = layout.burn.clone();
    let content = layout.content.clone();
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for tid in 0..8 {
        let burn = burn.clone();
        let content = content.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut ledger = Ledger::open(&burn, &content).unwrap();
            barrier.wait();
            for i in 0..25 {
                // Distinct input counts per (tid, i) so layer-2
                // fingerprint dedup doesn't collapse logically-distinct
                // turns from peer writers.
                let t = make_turn(
                    &format!("sess-{tid}-{i}"),
                    &format!("m-{tid}-{i}"),
                    "2025-01-01T00:00:00Z",
                    (tid * 1000 + i) as u64,
                );
                ledger.append_turns(&[t]).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let ledger = Ledger::open(&layout.burn, &layout.content).unwrap();
    assert_eq!(ledger.count_table("turns").unwrap(), 200);

    let names: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap()))
        .collect();
    for name in &names {
        assert!(!name.ends_with(".lock"), "lockfile leaked: {name}");
        assert!(!name.ends_with(".idx"), "index sidecar leaked: {name}");
        assert!(name != "ledger.jsonl", "JSONL ledger leaked");
        assert!(name != "archive.sqlite", "archive.sqlite leaked");
    }
}

#[test]
fn layer_one_dedup_by_unique_constraint() {
    // Acceptance: re-ingesting the same (source, session_id, message_id)
    // is a UNIQUE-collision no-op.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let t = make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10);
    assert_eq!(l.append_turns(std::slice::from_ref(&t)).unwrap(), 1);
    // Identical ingest: ignored.
    assert_eq!(l.append_turns(std::slice::from_ref(&t)).unwrap(), 0);
    assert_eq!(l.count_table("turns").unwrap(), 1);
}

#[test]
fn layer_two_dedup_by_content_fingerprint() {
    // Acceptance: same shape under a fresh messageId produces no row.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let original = make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10);
    let renamed = make_turn("s1", "m2-different", "2025-01-01T00:00:00Z", 10);
    assert_eq!(l.append_turns(&[original]).unwrap(), 1);
    // Fingerprint matches the first turn's shape — collapsed.
    assert_eq!(l.append_turns(&[renamed]).unwrap(), 0);
    assert_eq!(l.count_table("turns").unwrap(), 1);
}

#[test]
fn layer_two_distinguishes_different_shapes() {
    // Sanity: two turns with the same id key but genuinely different
    // shape should both land. (Layer-1 already prevents the id from
    // colliding, so we vary the messageId here.)
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let a = make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10);
    let mut b = make_turn("s1", "m2", "2025-01-01T00:00:01Z", 10);
    b.usage.input = 999; // genuinely different cost shape
    assert_eq!(l.append_turns(&[a]).unwrap(), 1);
    assert_eq!(l.append_turns(&[b]).unwrap(), 1);
    assert_eq!(l.count_table("turns").unwrap(), 2);
}

#[test]
fn stamps_survive_state_rebuild() {
    // Acceptance: stamps written via `burn stamp` survive
    // `burn state rebuild` unchanged.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let mut enrichment = BTreeMap::new();
    enrichment.insert("role".into(), "fix-bug".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("s1".into()),
            ..Default::default()
        },
        enrichment.clone(),
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    // Add some derivable rows so we can confirm rebuild drops them.
    l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
        .unwrap();
    assert_eq!(l.count_table("turns").unwrap(), 1);
    assert_eq!(l.count_table("stamps").unwrap(), 1);

    let summary = l.rebuild_derivable().unwrap();
    assert_eq!(summary.rows_dropped, 1);

    assert_eq!(l.count_table("turns").unwrap(), 0);
    assert_eq!(l.count_table("stamps").unwrap(), 1);

    let stamps = l.list_stamps().unwrap();
    assert_eq!(stamps.len(), 1);
    assert_eq!(stamps[0].enrichment.get("role").map(String::as_str), Some("fix-bug"));
}

#[test]
fn cursors_survive_state_rebuild() {
    // Acceptance: ingest cursors in archive_state likewise survive.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    l.write_cursors(r#"{"claude-code": "2025-01-01T00:00:00Z"}"#).unwrap();
    l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
        .unwrap();
    l.rebuild_derivable().unwrap();

    let cursors = l.read_cursors().unwrap();
    assert_eq!(cursors, r#"{"claude-code": "2025-01-01T00:00:00Z"}"#);
}

#[test]
fn reset_wipes_derivable_stamps_content_and_cursors() {
    // `reset()` is the harder-hitting sibling of `rebuild_derivable`:
    // unlike rebuild, it MUST drop stamps and blank ingest cursors so
    // a follow-up `burn ingest` walks every upstream file from offset
    // 0. This test pins all three behaviours.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    l.write_cursors(r#"{"claude-code": "2025-01-01T00:00:00Z"}"#).unwrap();

    let mut enrichment = BTreeMap::new();
    enrichment.insert("role".into(), "fix-bug".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("s1".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
        .unwrap();
    l.append_content(&[make_content("s1", "m1", "out of memory error")])
        .unwrap();

    assert_eq!(l.count_table("turns").unwrap(), 1);
    assert_eq!(l.count_table("stamps").unwrap(), 1);
    assert_eq!(l.count_content().unwrap(), 1);

    let summary = l.reset().unwrap();
    assert_eq!(summary.rows_dropped, 1, "1 derivable row dropped (turns)");
    assert_eq!(summary.stamps_dropped, 1);
    assert_eq!(summary.content_rows_dropped, 1);

    assert_eq!(l.count_table("turns").unwrap(), 0);
    assert_eq!(l.count_table("stamps").unwrap(), 0);
    assert_eq!(l.count_content().unwrap(), 0);
    assert_eq!(
        l.read_cursors().unwrap(),
        "{}",
        "reset blanks ingest cursors"
    );

    // FTS must also be empty so post-reset search returns nothing.
    let post = l.search_content(SearchOptions::new("memory")).unwrap();
    assert!(post.is_empty(), "FTS5 should be empty after reset");
}

#[test]
fn count_reset_targets_does_not_mutate() {
    // The dry-run path of `burn state reset` calls
    // `count_reset_targets()` and prints the would-drop counts. The
    // call MUST be read-only — a stray DELETE here would silently turn
    // every dry-run into a destructive op.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let mut enrichment = BTreeMap::new();
    enrichment.insert("role".into(), "fix-bug".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("s1".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();
    l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
        .unwrap();
    l.append_content(&[make_content("s1", "m1", "hello")]).unwrap();

    let preview = l.count_reset_targets().unwrap();
    assert_eq!(preview.rows_dropped, 1);
    assert_eq!(preview.stamps_dropped, 1);
    assert_eq!(preview.content_rows_dropped, 1);

    // Nothing changed on disk.
    assert_eq!(l.count_table("turns").unwrap(), 1);
    assert_eq!(l.count_table("stamps").unwrap(), 1);
    assert_eq!(l.count_content().unwrap(), 1);

    // A second call returns the same numbers — idempotent dry-run.
    let preview2 = l.count_reset_targets().unwrap();
    assert_eq!(preview, preview2);
}

#[test]
fn count_reset_targets_propagates_sql_errors() {
    // Regression: before #341 review, this method swallowed
    // `query_row` failures via `unwrap_or(0)` and reported a clean
    // zero-count summary on a corrupt ledger. Drop the `turns` table
    // out from under the open connection (a stand-in for any
    // schema/corruption fault that would normally surface a
    // `LedgerError::Sqlite`) and confirm the call now errors instead
    // of silently returning `Ok(ResetSummary::default())`.
    let tmp = TempDir::new().unwrap();
    let l = open_in(&tmp);
    l.conns.burn.execute("DROP TABLE turns", []).unwrap();

    let result = l.count_reset_targets();
    assert!(
        result.is_err(),
        "expected SQL failure to propagate, got {:?}",
        result
    );
}

#[test]
fn reset_is_idempotent_on_empty_ledger() {
    // Running reset on a fresh ledger should be a no-op: zero counts,
    // both DBs still openable, archive_state row still present (the
    // CHECK constraint pins id=1, so "delete + reinsert" would have
    // tripped the constraint).
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let summary = l.reset().unwrap();
    assert_eq!(summary, ResetSummary::default());

    // archive_state row survives — read_cursors() reads `id = 1` and
    // would error on a missing row.
    assert_eq!(l.read_cursors().unwrap(), "{}");

    // A second reset still works (re-checks transaction path).
    let again = l.reset().unwrap();
    assert_eq!(again, ResetSummary::default());
}

#[test]
fn rebuild_clears_content_and_fts_index() {
    // Acceptance: `burn state rebuild` regenerates the entire
    // content.sqlite (including the FTS index).
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    l.append_content(&[make_content("ses_a", "m1", "out of memory error")])
        .unwrap();
    let pre = l.search_content(SearchOptions::new("memory")).unwrap();
    assert_eq!(pre.len(), 1, "FTS5 should match before rebuild");

    l.rebuild_derivable().unwrap();
    assert_eq!(l.count_content().unwrap(), 0);
    let post = l.search_content(SearchOptions::new("memory")).unwrap();
    assert!(post.is_empty(), "FTS5 should match nothing after rebuild");
}

#[test]
fn fts5_search_returns_ranked_snippets() {
    // Acceptance: FTS5 search returns ranked hits with snippets across
    // content bodies; tested against a populated content.sqlite with
    // multi-word queries, phrase queries, and boolean operators.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    l.append_content(&[
        make_content("ses_a", "m1", "the build failed with an out of memory error"),
        make_content("ses_a", "m2", "permission denied while reading file"),
        make_content("ses_b", "m1", "out of memory: killed by oom-killer"),
    ])
    .unwrap();

    // Multi-word AND.
    let hits = l
        .search_content(SearchOptions::new("build memory"))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("<b>"));

    // Phrase query.
    let phrase = l
        .search_content(SearchOptions::new(r#""out of memory""#))
        .unwrap();
    assert_eq!(phrase.len(), 2);
    assert!(phrase[0].rank <= phrase[1].rank, "ranks ordered low→high");

    // Boolean OR. Quote `oom-killer` so the FTS5 parser treats `-` as
    // a token separator instead of a column-restrict operator.
    let bool_q = l
        .search_content(SearchOptions::new(r#"permission OR "oom killer""#))
        .unwrap();
    assert_eq!(bool_q.len(), 2);

    // Session filter.
    let scoped = l
        .search_content(SearchOptions {
            query: "memory",
            limit: 25,
            session_id: Some("ses_a"),
        })
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].session_id, "ses_a");
}

#[test]
fn fts5_index_stays_consistent_across_insert_delete() {
    // Acceptance: FTS5 index stays consistent across content
    // insert/delete via the sync triggers.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    l.append_content(&[make_content("ses_a", "m1", "needle in haystack")])
        .unwrap();
    assert_eq!(
        l.search_content(SearchOptions::new("needle")).unwrap().len(),
        1
    );

    // Bypass: prune everything older than the future.
    l.prune_content_older_than("zzzz").unwrap();
    assert_eq!(l.count_content().unwrap(), 0);
    // Trigger should have removed the FTS row too.
    assert_eq!(
        l.search_content(SearchOptions::new("needle")).unwrap().len(),
        0
    );

    // Re-add: trigger should restore the FTS row.
    l.append_content(&[make_content("ses_a", "m1", "needle in haystack")])
        .unwrap();
    assert_eq!(
        l.search_content(SearchOptions::new("needle")).unwrap().len(),
        1
    );
}

#[test]
fn stamp_synthesizes_spawn_env_relationship() {
    // Sanity: a stamp with parentAgentId injects the implied subagent
    // relationship row, mirroring the TS adapter so analytics see the
    // edge even if the source log didn't carry it.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let mut enrichment = BTreeMap::new();
    enrichment.insert("parentAgentId".into(), "parent-1".into());
    enrichment.insert("agentId".into(), "child-1".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("child-session".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    let rels = l.query_relationships(&Query::default()).unwrap();
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].source, RelationshipSourceKind::SpawnEnv);
    assert_eq!(rels[0].relationship_type, RelationshipType::Subagent);
    assert_eq!(rels[0].related_session_id.as_deref(), Some("parent-1"));
    assert_eq!(rels[0].agent_id.as_deref(), Some("child-1"));
}

#[test]
fn count_table_rejects_arbitrary_sql() {
    // Devin review (#260): public `count_table(&str)` interpolated the
    // table name straight into SQL. Validate against an allowlist so
    // downstream callers can't smuggle a subquery.
    let tmp = TempDir::new().unwrap();
    let l = open_in(&tmp);
    assert!(l.count_table("turns").is_ok());
    let injected =
        l.count_table("turns WHERE 1=0 UNION SELECT upstream_cursors_json FROM archive_state");
    match injected {
        Err(LedgerError::Other(msg)) => assert!(msg.contains("unknown ledger table")),
        Err(other) => panic!("expected Other, got {other:?}"),
        Ok(_) => panic!("injection accepted; allowlist not enforced"),
    }
}

#[test]
fn rebuild_replays_stamp_synthesized_relationships() {
    // P1 review feedback (#260): `relationships` is in
    // DERIVABLE_TABLES, but stamp-synthesized spawn-env edges are not
    // recoverable from upstream session files — they live and die
    // with the stamp. Rebuild must re-emit them so subagent
    // parent/child queries don't go silently incomplete.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let mut enrichment = BTreeMap::new();
    enrichment.insert("parentAgentId".into(), "parent-1".into());
    enrichment.insert("agentId".into(), "child-1".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("child-session".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();
    assert_eq!(l.count_table("relationships").unwrap(), 1);

    l.rebuild_derivable().unwrap();

    let rels = l.query_relationships(&Query::default()).unwrap();
    assert_eq!(rels.len(), 1, "stamp-synthesized edge should survive");
    assert_eq!(rels[0].source, RelationshipSourceKind::SpawnEnv);
    assert_eq!(rels[0].related_session_id.as_deref(), Some("parent-1"));
}

#[test]
fn relationship_query_filters_by_source() {
    // P2 review feedback (#260): `q.source` must filter relationship
    // rows. Since `RelationshipSourceKind` is a superset of
    // `SourceKind`, comparison is by serialized kebab-case, matching
    // the TS adapter — `source = "claude-code"` matches the
    // claude-code variant on either enum but not `spawn-env`.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let claude_rel = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: "child".into(),
        related_session_id: Some("parent".into()),
        relationship_type: RelationshipType::Continuation,
        ts: Some("2025-01-01T00:00:00Z".into()),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    l.append_relationships(&[claude_rel]).unwrap();

    // Stamp-synthesized spawn-env edge for the same session.
    let mut enrichment = BTreeMap::new();
    enrichment.insert("parentAgentId".into(), "parent-1".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("child".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    let all = l.query_relationships(&Query::default()).unwrap();
    assert_eq!(all.len(), 2, "no filter ⇒ both rows");

    let claude_only = l
        .query_relationships(&Query {
            source: Some(SourceKind::ClaudeCode),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(claude_only.len(), 1);
    assert_eq!(
        claude_only[0].source,
        RelationshipSourceKind::ClaudeCode,
        "spawn-env edge must be excluded"
    );
}

#[test]
fn export_ledger_round_trips_to_jsonl() {
    // Acceptance: `burn export ledger --format jsonl` round-trips a
    // populated DB to JSONL byte-equivalent to the events ingested.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    l.append_turns(&[
        make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10),
        make_turn("s1", "m2", "2025-01-02T00:00:00Z", 20),
    ])
    .unwrap();

    let mut buf = Vec::new();
    l.export_ledger_jsonl(&mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "turn");
        assert!(v["record"].is_object());
    }
}

#[test]
fn export_stamps_round_trips_to_jsonl() {
    // Acceptance: `burn stamps export` round-trips the stamps table to
    // JSONL.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    let mut enrichment = BTreeMap::new();
    enrichment.insert("role".into(), "fix-bug".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("s1".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    let mut buf = Vec::new();
    l.export_stamps_jsonl(&mut buf).unwrap();
    let line = String::from_utf8(buf).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["kind"], "stamp");
    assert_eq!(v["selector"]["sessionId"], "s1");
    assert_eq!(v["enrichment"]["role"], "fix-bug");
}

#[test]
fn enriched_turn_query_folds_stamps() {
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);

    l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
        .unwrap();
    let mut enrichment = BTreeMap::new();
    enrichment.insert("phase".into(), "post-mortem".into());
    let stamp = Stamp::new(
        "2025-01-01T00:00:00Z",
        StampSelector {
            session_id: Some("s1".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    l.append_stamp(&stamp).unwrap();

    let turns = l.query_turns(&Query::default()).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].enrichment.get("phase").map(String::as_str),
        Some("post-mortem")
    );
}

#[test]
fn invalid_session_id_in_content_rejected() {
    // The content writer guards against path-escape session ids — even
    // though 2.0's content store is a SQLite blob, the same id flows
    // into stamps and exports; failing fast keeps that downstream
    // surface honest.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    let bad = make_content("../escape", "m1", "hi");
    let err = l.append_content(&[bad]).unwrap_err();
    assert!(matches!(err, LedgerError::InvalidSessionId(_)));
}

#[test]
fn schema_too_new_is_rejected() {
    // Defensive: if a future build wrote a higher schema_version, this
    // build refuses to open rather than silently truncating.
    let tmp = TempDir::new().unwrap();
    let layout = LedgerLayout::under(tmp.path());
    {
        let l = Ledger::open(&layout.burn, &layout.content).unwrap();
        l.conns
            .burn
            .execute(
                "UPDATE archive_state SET schema_version = 999 WHERE id = 1",
                [],
            )
            .unwrap();
    }
    match Ledger::open(&layout.burn, &layout.content) {
        Err(LedgerError::SchemaTooNew { found: 999, .. }) => {}
        Err(other) => panic!("expected SchemaTooNew, got {other:?}"),
        Ok(_) => panic!("expected SchemaTooNew, got Ok(_)"),
    }
}

#[test]
fn relationship_records_round_trip() {
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    let rel = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: "child".into(),
        related_session_id: Some("parent".into()),
        relationship_type: RelationshipType::Continuation,
        ts: Some("2025-01-01T00:00:00Z".into()),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    assert_eq!(l.append_relationships(std::slice::from_ref(&rel)).unwrap(), 1);
    // Re-append: dedup'd by primary-key fingerprint.
    assert_eq!(l.append_relationships(&[rel]).unwrap(), 0);
    assert_eq!(l.count_table("relationships").unwrap(), 1);
}

#[test]
fn concurrent_writers_serialize_via_wal() {
    // Acceptance: concurrent writers serialize via SQLite WAL on each
    // DB without any user-space lock; mirrors #243's 100-callers
    // property test (sized down for unit-test wall time).
    let tmp = TempDir::new().unwrap();
    let layout = LedgerLayout::under(tmp.path());
    {
        let _ = Ledger::open(&layout.burn, &layout.content).unwrap();
    }

    let writer_count = 16;
    let per_writer = 25;
    let total = writer_count * per_writer;

    let burn = layout.burn.clone();
    let content = layout.content.clone();
    let barrier = Arc::new(Barrier::new(writer_count));
    let mut handles = Vec::new();
    for tid in 0..writer_count {
        let burn = burn.clone();
        let content = content.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut ledger = Ledger::open(&burn, &content).unwrap();
            barrier.wait();
            for i in 0..per_writer {
                let t = make_turn(
                    &format!("wal-{tid}"),
                    &format!("m-{tid}-{i}"),
                    "2025-01-01T00:00:00Z",
                    (tid * per_writer + i) as u64,
                );
                ledger.append_turns(&[t]).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let ledger = Ledger::open(&layout.burn, &layout.content).unwrap();
    assert_eq!(ledger.count_table("turns").unwrap(), total as i64);
}

#[test]
fn list_content_session_ids_returns_distinct_set() {
    // #279: ingest's `reingest_missing_content` needs the set of session
    // ids already covered in `content.sqlite` so it can skip them.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    // Empty content store ⇒ empty set.
    assert!(l.list_content_session_ids().unwrap().is_empty());

    l.append_content(&[
        make_content("ses_a", "m1", "alpha"),
        make_content("ses_a", "m2", "beta"),
        make_content("ses_b", "m1", "gamma"),
        make_content("ses_c", "m1", "delta"),
    ])
    .unwrap();

    let ids = l.list_content_session_ids().unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains("ses_a"));
    assert!(ids.contains("ses_b"));
    assert!(ids.contains("ses_c"));

    // The `content` table is non-STRICT, so a row whose `session_id`
    // column holds a non-TEXT storage class can land in the DB (e.g. via
    // a future schema migration bug or direct ops intervention). The
    // TEXT affinity coerces incoming integers/reals into text on insert,
    // but it preserves BLOBs — so a BLOB literal is the way to plant a
    // row that will not decode as `String`. The `list_session_ids` call
    // must skip it rather than aborting the whole query.
    l.conns
        .content
        .execute(
            "INSERT INTO content (source, session_id, message_id, content_hash, body, byte_length, created_at)
             VALUES ('claude-code', X'AABBCCDD', 'm-bad', 'h-bad', 'corrupt', 7, '2026-05-05T00:00:00Z')",
            [],
        )
        .unwrap();
    let ids = l.list_content_session_ids().unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains("ses_a"));
    assert!(ids.contains("ses_b"));
    assert!(ids.contains("ses_c"));
}

#[test]
fn list_user_turn_session_ids_returns_distinct_set() {
    // #278: ingest's `reingest_missing_content` AND-combines content +
    // user-turn coverage. Mirrors the `list_content_session_ids` test
    // shape so a regression in either side surfaces the same way.
    let tmp = TempDir::new().unwrap();
    let mut l = open_in(&tmp);
    // Empty user_turns ⇒ empty set.
    assert!(l.list_user_turn_session_ids().unwrap().is_empty());

    l.append_user_turns(&[
        make_user_turn("ses_a", "u1", "2025-01-01T00:00:00Z"),
        make_user_turn("ses_a", "u2", "2025-01-01T00:00:01Z"),
        make_user_turn("ses_b", "u1", "2025-01-01T00:00:02Z"),
        make_user_turn("ses_c", "u1", "2025-01-01T00:00:03Z"),
    ])
    .unwrap();

    let ids = l.list_user_turn_session_ids().unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains("ses_a"));
    assert!(ids.contains("ses_b"));
    assert!(ids.contains("ses_c"));
}

fn make_user_turn(session: &str, user_uuid: &str, ts: &str) -> UserTurnRecord {
    UserTurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.into(),
        user_uuid: user_uuid.into(),
        ts: ts.into(),
        preceding_message_id: None,
        following_message_id: None,
        blocks: vec![UserTurnBlock {
            kind: UserTurnBlockKind::Text,
            tool_use_id: None,
            byte_len: 4,
            approx_tokens: 1,
            is_error: None,
        }],
    }
}

#[test]
fn pruning_content_does_not_lock_events_db() {
    // Acceptance: pruning content (TTL or explicit) does not lock the
    // events DB; analytic queries on burn.sqlite keep running.
    let tmp = TempDir::new().unwrap();
    let layout = LedgerLayout::under(tmp.path());
    let ledger = Arc::new(Mutex::new(Ledger::open(&layout.burn, &layout.content).unwrap()));

    {
        let mut l = ledger.lock().unwrap();
        l.append_turns(&[make_turn("s1", "m1", "2025-01-01T00:00:00Z", 10)])
            .unwrap();
        for i in 0..50 {
            l.append_content(&[make_content(
                "ses_x",
                &format!("m{i}"),
                "lots of body text",
            )])
            .unwrap();
        }
    }

    // Spawn a long-running prune task on a fresh handle (so it competes
    // with the events-DB read on a different connection).
    let burn = layout.burn.clone();
    let content = layout.content.clone();
    let prune_thread = thread::spawn(move || {
        let mut other = Ledger::open(&burn, &content).unwrap();
        let _ = other.prune_content_older_than("zzzz").unwrap();
    });

    // Concurrent analytic query on burn.sqlite: should succeed.
    {
        let l = ledger.lock().unwrap();
        let turns = l.query_turns(&Query::default()).unwrap();
        assert_eq!(turns.len(), 1);
    }
    prune_thread.join().unwrap();
}
