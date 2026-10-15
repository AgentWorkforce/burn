//! `turns.request_count` and the re-accounting of Codex rows that predate
//! it.

use tempfile::TempDir;

use super::*;
use crate::reader::{build_inferences, Inference, SourceKind, TurnRecord, Usage};

fn open_in(tmp: &TempDir) -> Ledger {
    let layout = LedgerLayout::under(tmp.path());
    Ledger::open(&layout.burn, &layout.content).unwrap()
}

/// A Codex turn; its spend differs per session and message so no two are
/// one turn to the ledger's content fingerprint.
fn codex_turn(session: &str, message: &str, request_count: Option<u64>) -> TurnRecord {
    let input = format!("{session}/{message}")
        .bytes()
        .fold(0u64, |acc, b| acc * 31 % 1_000_003 + u64::from(b));
    serde_json::from_value(serde_json::json!({
        "v": 1, "source": "codex", "sessionId": session, "messageId": message,
        "turnIndex": 0, "requestCount": request_count,
        "ts": "2026-04-20T00:00:00.000Z", "model": "gpt-5.4",
        "usage": {"input": input, "output": 120, "reasoning": 0, "cacheRead": 400,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": [],
    }))
    .unwrap()
}

fn request(turn: &TurnRecord, n: u64, input: u64) -> Inference {
    let mut row = build_inferences(std::slice::from_ref(turn), &Default::default())
        .pop()
        .unwrap();
    row.request_id = format!("{}:request:{n:06}", turn.message_id);
    row.usage = Usage {
        input,
        ..Usage::default()
    };
    row
}

/// `(message id, request_count column, record's requestCount)` per turn.
fn counts(ledger: &Ledger) -> Vec<(String, Option<i64>, Option<u64>)> {
    let mut stmt = ledger
        .conns
        .burn
        .prepare("SELECT message_id, request_count, record_json FROM turns ORDER BY message_id")
        .unwrap();
    stmt.query_map([], |r| {
        let json: String = r.get(2)?;
        let record: TurnRecord = serde_json::from_str(&json).unwrap();
        Ok((r.get(0)?, r.get(1)?, record.request_count))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn request_ids(ledger: &Ledger) -> Vec<String> {
    let mut ids: Vec<String> = ledger
        .query_inferences(&Query::default())
        .unwrap()
        .into_iter()
        .map(|i| i.request_id)
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_request_count_column_mirrors_the_record() {
    let tmp = TempDir::new().unwrap();
    let mut ledger = open_in(&tmp);
    ledger
        .append_turns(&[
            codex_turn("s", "counted", Some(3)),
            codex_turn("s", "legacy", None),
        ])
        .unwrap();
    assert_eq!(
        counts(&ledger),
        [
            ("counted".to_string(), Some(3), Some(3)),
            ("legacy".to_string(), None, None),
        ]
    );
}

#[test]
fn only_codex_sessions_with_uncounted_turns_await_request_accounting() {
    let tmp = TempDir::new().unwrap();
    let mut ledger = open_in(&tmp);
    let mut claude = codex_turn("claude", "c1", None);
    claude.source = SourceKind::ClaudeCode;
    ledger
        .append_turns(&[
            codex_turn("b-legacy", "t1", None),
            codex_turn("b-legacy", "t2", Some(2)),
            codex_turn("a-legacy", "t3", None),
            codex_turn("counted", "t4", Some(1)),
            claude,
        ])
        .unwrap();
    assert_eq!(
        ledger.sessions_awaiting_request_accounting().unwrap(),
        ["a-legacy", "b-legacy"]
    );
    assert!(ledger.awaits_request_accounting("b-legacy").unwrap());
    assert!(!ledger.awaits_request_accounting("counted").unwrap());
    assert!(!ledger.awaits_request_accounting("claude").unwrap());
    assert!(!ledger.awaits_request_accounting("absent").unwrap());
}

#[test]
fn rewriting_replaces_uncounted_turns_and_their_inferences_in_place() {
    let tmp = TempDir::new().unwrap();
    let mut ledger = open_in(&tmp);
    let legacy = codex_turn("s", "t1", None);
    let counted = codex_turn("s", "t2", Some(1));
    ledger
        .append_turns(&[legacy.clone(), counted.clone()])
        .unwrap();
    let held = build_inferences(&[legacy.clone(), counted.clone()], &Default::default());
    ledger.append_inferences(&held).unwrap();
    assert_eq!(request_ids(&ledger), ["t1", "t2"]);

    let mut recounted = legacy.clone();
    recounted.request_count = Some(2);
    let mut changed = counted.clone();
    changed.request_count = Some(5);
    let unheld = codex_turn("s", "t3", Some(1));
    let rows = [
        request(&recounted, 1, 200),
        request(&recounted, 2, 400),
        request(&changed, 1, 600),
        request(&unheld, 1, 600),
    ];
    let rewritten = ledger
        .rewrite_turns(&[recounted.clone(), changed, unheld], &rows)
        .unwrap();

    assert_eq!(rewritten, 1, "only the uncounted held turn is rewritten");
    assert_eq!(
        counts(&ledger),
        [
            ("t1".to_string(), Some(2), Some(2)),
            ("t2".to_string(), Some(1), Some(1)),
        ]
    );
    assert_eq!(
        request_ids(&ledger),
        ["t1:request:000001", "t1:request:000002", "t2"],
        "the rewritten turn's task row gives way to its request rows"
    );
    let turns = ledger.query_turns(&Query::for_session("s")).unwrap();
    assert_eq!(turns[0].turn.usage, legacy.usage, "spend is untouched");
    assert!(!ledger.awaits_request_accounting("s").unwrap());
}

#[test]
fn rewriting_nothing_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let mut ledger = open_in(&tmp);
    ledger.append_turns(&[codex_turn("s", "t1", None)]).unwrap();
    ledger
        .conns
        .burn
        .execute("UPDATE archive_state SET last_write_at_ms = 1", [])
        .unwrap();
    assert_eq!(ledger.rewrite_turns(&[], &[]).unwrap(), 0);
    let other = codex_turn("s", "elsewhere", Some(1));
    assert_eq!(ledger.rewrite_turns(&[other], &[]).unwrap(), 0);
    assert_eq!(ledger.last_write_at_ms().unwrap(), Some(1));
    assert_eq!(counts(&ledger), [("t1".to_string(), None, None)]);
}

#[test]
fn a_schema_8_ledger_gains_the_request_count_column() {
    let tmp = TempDir::new().unwrap();
    let layout = LedgerLayout::under(tmp.path());
    {
        let mut ledger = Ledger::open(&layout.burn, &layout.content).unwrap();
        ledger.append_turns(&[codex_turn("s", "t1", None)]).unwrap();
        ledger
            .conns
            .burn
            .execute_batch(
                "DROP INDEX idx_turns_unaccounted_codex;
                 ALTER TABLE turns DROP COLUMN request_count;
                 UPDATE archive_state SET schema_version = 8;",
            )
            .unwrap();
    }
    let ledger = Ledger::open(&layout.burn, &layout.content).unwrap();
    let version: i64 = ledger
        .conns
        .burn
        .query_row("SELECT schema_version FROM archive_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 9);
    assert_eq!(counts(&ledger), [("t1".to_string(), None, None)]);
    assert_eq!(
        ledger.sessions_awaiting_request_accounting().unwrap(),
        ["s"]
    );
}
