//! JSONL ledger append + stream-parse, mirroring
//! `packages/ledger/src/adapters/file-adapter.ts`.
//!
//! Highlights ported faithfully from the TS:
//!   - `append_turns` performs *batch-snapshot* dedup: id hashes dedupe
//!     within the batch, but content fingerprints are compared only against
//!     what's already on disk. Two turns in the *same* batch that share a
//!     content fingerprint both land — that's by design (rapid back-to-back
//!     duplicate-shape turns are common and must not be collapsed).
//!   - All appends hold the `ledger` lock so a reclassify pass can't
//!     interleave a read-modify-write with our append.

use std::path::Path;

use serde_json::{Deserializer, Value};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::errors::Result;
use crate::lock::with_lock;
use crate::paths::ledger_path;
use crate::schema::{
    compaction_id_hash, relationship_id_hash, tool_result_event_id_hash,
    turn_content_fingerprint, turn_id_hash, user_turn_id_hash, LedgerLine,
};
use crate::sidecar::{append_hashes, load_index};

/// Append a batch of `turn` records, deduping by id-hash and content
/// fingerprint. Returns the number of records actually written.
pub async fn append_turns(records: &[Value]) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let idx = load_index().await?;
    // Snapshot of historical content set: content-fingerprint dedup only
    // compares against historically-committed turns, never within the same
    // batch.
    let historical_content = idx.content.clone();
    let mut ids_seen_in_batch = idx.ids.clone();

    let mut fresh: Vec<&Value> = Vec::new();
    let mut new_ids: Vec<String> = Vec::new();
    let mut new_content: Vec<String> = Vec::new();
    for r in records {
        let id = turn_id_hash(r);
        if ids_seen_in_batch.contains(&id) {
            continue;
        }
        let cf = turn_content_fingerprint(r);
        if historical_content.contains(&cf) {
            continue;
        }
        ids_seen_in_batch.insert(id.clone());
        new_ids.push(id);
        new_content.push(cf);
        fresh.push(r);
    }
    if fresh.is_empty() {
        return Ok(0);
    }

    let lines: Vec<LedgerLine> = fresh
        .into_iter()
        .map(|record| LedgerLine::turn(record.clone()))
        .collect();
    append_lines(&lines).await?;
    append_hashes(&new_ids, &new_content).await?;
    Ok(lines.len())
}

/// Append compaction events with id-hash dedup. Returns count actually written.
pub async fn append_compactions(records: &[Value]) -> Result<usize> {
    append_dedup(records, "compaction", compaction_id_hash, |r| {
        LedgerLine {
            v: 1,
            body: crate::schema::LineKind::Compaction { record: r.clone() },
        }
    })
    .await
}

pub async fn append_relationships(records: &[Value]) -> Result<usize> {
    append_dedup(records, "relationship", relationship_id_hash, |r| {
        LedgerLine {
            v: 1,
            body: crate::schema::LineKind::Relationship { record: r.clone() },
        }
    })
    .await
}

pub async fn append_tool_result_events(records: &[Value]) -> Result<usize> {
    append_dedup(records, "tool_result_event", tool_result_event_id_hash, |r| {
        LedgerLine {
            v: 1,
            body: crate::schema::LineKind::ToolResultEvent { record: r.clone() },
        }
    })
    .await
}

pub async fn append_user_turns(records: &[Value]) -> Result<usize> {
    append_dedup(records, "user_turn", user_turn_id_hash, |r| LedgerLine {
        v: 1,
        body: crate::schema::LineKind::UserTurn { record: r.clone() },
    })
    .await
}

async fn append_dedup<H, B>(
    records: &[Value],
    _kind: &str,
    id_hash: H,
    build: B,
) -> Result<usize>
where
    H: Fn(&Value) -> String,
    B: Fn(&Value) -> LedgerLine,
{
    if records.is_empty() {
        return Ok(0);
    }
    let idx = load_index().await?;
    let mut seen = idx.ids.clone();

    let mut fresh: Vec<&Value> = Vec::new();
    let mut new_ids: Vec<String> = Vec::new();
    for r in records {
        let id = id_hash(r);
        if seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());
        new_ids.push(id);
        fresh.push(r);
    }
    if fresh.is_empty() {
        return Ok(0);
    }
    let lines: Vec<LedgerLine> = fresh.into_iter().map(&build).collect();
    append_lines(&lines).await?;
    append_hashes(&new_ids, &[]).await?;
    Ok(lines.len())
}

/// Append a stamp line directly. No dedup happens at the ledger level for
/// stamps; the TS implementation also opportunistically materializes a
/// `subagent` relationship row when the stamp carries `parentAgentId`, which
/// we don't model yet (the relationship records ride on the
/// reader-port-typed `SessionRelationshipRecord`). When the reader port
/// lands the synthesized line will move here.
pub async fn append_stamp(line: LedgerLine) -> Result<()> {
    if line.as_stamp().is_none() {
        // Caller passed something that isn't a stamp. Be lenient — the TS
        // adapter assumes `StampLine` typing, but here we treat it as a
        // regular append.
    }
    append_lines(&[line]).await
}

async fn append_lines(lines: &[LedgerLine]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let path = ledger_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut payload = String::new();
    for l in lines {
        payload.push_str(&serde_json::to_string(l)?);
        payload.push('\n');
    }

    with_lock("ledger", || async move {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        f.write_all(payload.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    })
    .await
}

/// Stream-parse `ledger.jsonl`, yielding each `LedgerLine` in file order.
/// Mirrors the `streamLines` helper in `file-adapter.ts`. Lines that fail to
/// parse are silently skipped (same behavior as TS).
pub async fn read_all_lines(path: &Path) -> Result<Vec<LedgerLine>> {
    if !fs::try_exists(path).await? {
        return Ok(Vec::new());
    }
    let mut f = fs::File::open(path).await?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;

    // serde_json::Deserializer::from_slice().into_iter() is the same shape
    // the issue calls out for the archive's tail loop (#243). It avoids the
    // per-line String allocation a manual `split('\n')` would produce.
    let mut out = Vec::new();
    for value in Deserializer::from_slice(&buf).into_iter::<LedgerLine>() {
        match value {
            Ok(l) => out.push(l),
            Err(_) => continue,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::set_home;
    use serde_json::json;
    use tempfile::tempdir;

    async fn fresh_home() -> (tempfile::TempDir, tokio::sync::MutexGuard<'static, ()>) {
        let dir = tempdir().unwrap();
        let g = set_home(dir.path()).await;
        (dir, g)
    }

    fn make_turn(message_id: &str) -> Value {
        json!({
            "source": "claude",
            "sessionId": "s1",
            "messageId": message_id,
            "ts": format!("2026-01-01T00:00:0{}Z", message_id.chars().last().unwrap()),
            "model": "claude-sonnet-4-6",
            "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheCreate5m": 0, "cacheCreate1h": 0},
            "toolCalls": []
        })
    }

    #[tokio::test]
    async fn appends_and_round_trips_turns() {
        let (_dir, _g) = fresh_home().await;
        let n = append_turns(&[make_turn("a"), make_turn("b")])
            .await
            .unwrap();
        assert_eq!(n, 2);

        let lines = read_all_lines(&ledger_path()).await.unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].as_turn().is_some());
    }

    #[tokio::test]
    async fn dedupes_by_id_within_repeated_calls() {
        let (_dir, _g) = fresh_home().await;
        let t = make_turn("a");
        let n1 = append_turns(&[t.clone()]).await.unwrap();
        let n2 = append_turns(&[t.clone()]).await.unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 0);

        let lines = read_all_lines(&ledger_path()).await.unwrap();
        assert_eq!(lines.len(), 1);
    }

    #[tokio::test]
    async fn dedups_compactions() {
        let (_dir, _g) = fresh_home().await;
        let e = json!({
            "source": "claude",
            "sessionId": "s1",
            "ts": "2026-01-01T00:00:00Z",
        });
        assert_eq!(append_compactions(&[e.clone()]).await.unwrap(), 1);
        assert_eq!(append_compactions(&[e.clone()]).await.unwrap(), 0);
    }
}
