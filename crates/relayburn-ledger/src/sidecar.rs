//! In-memory + on-disk hash index for ledger dedup.
//!
//! Mirrors `packages/ledger/src/index-sidecar.ts`. Two side files live next
//! to `ledger.jsonl`:
//!   - `ledger.idx`           — every TurnRecord/CompactionEvent/etc. id
//!     hash ever appended; consulted by writers to skip duplicates.
//!   - `ledger.content.idx`   — rolling window of `CONTENT_WINDOW`
//!     turnContentFingerprints; lets us detect "same usage shape, different
//!     messageId" turns that would otherwise re-ingest after a session id
//!     change.
//!
//! The file format is one hex hash per line. Both readers and writers must
//! hold the `ledger-index` lock to avoid torn appends.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Deserializer;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::errors::Result;
use crate::lock::with_lock;
use crate::paths::{
    ledger_content_index_path, ledger_home, ledger_index_path, ledger_path,
};
use crate::schema::{
    compaction_id_hash, relationship_id_hash, tool_result_event_id_hash,
    turn_content_fingerprint, turn_id_hash, user_turn_id_hash, LedgerLine,
};

pub const CONTENT_WINDOW: usize = 10_000;

#[derive(Debug, Default, Clone)]
pub struct IndexSnapshot {
    pub ids: HashSet<String>,
    pub content: HashSet<String>,
    pub content_order: Vec<String>,
    pub home: std::path::PathBuf,
}

impl IndexSnapshot {
    fn empty(home: std::path::PathBuf) -> Self {
        Self {
            ids: HashSet::new(),
            content: HashSet::new(),
            content_order: Vec::new(),
            home,
        }
    }
}

// Cache keyed on `RELAYBURN_HOME` so swapping the env between tests / runs
// invalidates automatically. `OnceLock` would be nicer than `Lazy` here
// but we need *re-initialization* when the home changes, which a Mutex
// gives us cheaply.
static CACHE: Mutex<Option<IndexSnapshot>> = Mutex::const_new(None);

/// Drop the in-memory dedup cache. Callers that wipe the on-disk index
/// (e.g. `burn state reset`) MUST call this so the next `load_index` re-reads
/// from the empty files instead of returning hashes loaded before the wipe.
pub async fn invalidate_index_cache() {
    let mut guard = CACHE.lock().await;
    *guard = None;
}

/// Return a snapshot of the dedup index (id hashes + rolling content
/// fingerprints), reading from disk on first call after a `RELAYBURN_HOME`
/// change.
pub async fn load_index() -> Result<IndexSnapshot> {
    let home = ledger_home();
    {
        let guard = CACHE.lock().await;
        if let Some(snap) = guard.as_ref() {
            if snap.home == home {
                return Ok(snap.clone());
            }
        }
    }

    let ids = load_hashes(&ledger_index_path()).await?;
    let content_lines = load_hashes_array(&ledger_content_index_path()).await?;
    let tail_start = content_lines.len().saturating_sub(CONTENT_WINDOW);
    let tail: Vec<String> = content_lines[tail_start..].to_vec();
    let content: HashSet<String> = tail.iter().cloned().collect();

    let snap = IndexSnapshot {
        ids,
        content,
        content_order: tail,
        home,
    };
    let mut guard = CACHE.lock().await;
    *guard = Some(snap.clone());
    Ok(snap)
}

/// Append id-hashes (and rolling content fingerprints) to the on-disk index.
/// Holds the `ledger-index` lock so concurrent writers don't tear the file.
pub async fn append_hashes(id_hashes: &[String], content_hashes: &[String]) -> Result<()> {
    if id_hashes.is_empty() && content_hashes.is_empty() {
        return Ok(());
    }
    if let Some(parent) = ledger_index_path().parent() {
        fs::create_dir_all(parent).await?;
    }
    let id_hashes = id_hashes.to_vec();
    let content_hashes = content_hashes.to_vec();
    with_lock("ledger-index", || async move {
        if !id_hashes.is_empty() {
            append_lines(&ledger_index_path(), &id_hashes).await?;
        }
        // Pull the cached snapshot so we can update both the on-disk content
        // tail and the in-memory ids/content sets atomically. Without this
        // refresh, the next `load_index()` (cache hit) would miss the
        // hashes we just wrote, and back-to-back `append_*` calls would
        // double-write.
        let mut snap = load_index().await?;
        for id in &id_hashes {
            snap.ids.insert(id.clone());
        }
        if !content_hashes.is_empty() {
            for h in &content_hashes {
                snap.content_order.push(h.clone());
                snap.content.insert(h.clone());
            }
            if snap.content_order.len() > CONTENT_WINDOW {
                let drop_n = snap.content_order.len() - CONTENT_WINDOW;
                snap.content_order.drain(..drop_n);
                snap.content = snap.content_order.iter().cloned().collect();
                let p = ledger_content_index_path();
                let tmp = p.with_extension("content.idx.tmp");
                let body = if snap.content_order.is_empty() {
                    String::new()
                } else {
                    let mut s = snap.content_order.join("\n");
                    s.push('\n');
                    s
                };
                fs::write(&tmp, body.as_bytes()).await?;
                fs::rename(&tmp, &p).await?;
            } else {
                append_lines(&ledger_content_index_path(), &content_hashes).await?;
            }
        }
        let mut guard = CACHE.lock().await;
        *guard = Some(snap);
        Ok(())
    })
    .await
}

/// Walk `ledger.jsonl` end-to-end and rebuild both index files. The hot loop
/// uses `serde_json::Deserializer::from_reader().into_iter()` (the same shape
/// the issue calls out for the archive's tail loop) so we don't have to
/// materialize each line into a String first.
pub async fn rebuild_index() -> Result<RebuildReport> {
    let ledger = ledger_path();
    let mut ids = HashSet::<String>::new();
    let mut content_order = Vec::<String>::new();
    let mut content_seen = HashSet::<String>::new();

    if let Ok(meta) = fs::metadata(&ledger).await {
        if meta.is_file() {
            // serde_json::Deserializer streams; do this in a blocking thread
            // so async runtimes aren't pinned by per-line work on giant
            // ledgers.
            let path = ledger.clone();
            let parsed: Vec<LedgerLine> = tokio::task::spawn_blocking(move || -> Result<_> {
                let f = std::fs::File::open(&path)?;
                let reader = std::io::BufReader::new(f);
                let mut lines = Vec::new();
                for value in Deserializer::from_reader(reader).into_iter::<LedgerLine>() {
                    match value {
                        Ok(line) => lines.push(line),
                        Err(_) => continue, // skip malformed
                    }
                }
                Ok(lines)
            })
            .await
            .expect("blocking task panicked")?;

            for line in parsed {
                if let Some(t) = line.as_turn() {
                    ids.insert(turn_id_hash(t.record));
                    let cf = turn_content_fingerprint(t.record);
                    if !content_seen.contains(&cf) {
                        content_seen.insert(cf.clone());
                        content_order.push(cf);
                    }
                } else if let Some(c) = line.as_compaction() {
                    ids.insert(compaction_id_hash(c.record));
                } else if let Some(r) = line.as_relationship() {
                    ids.insert(relationship_id_hash(r.record));
                } else if let Some(e) = line.as_tool_result_event() {
                    ids.insert(tool_result_event_id_hash(e.record));
                } else if let Some(u) = line.as_user_turn() {
                    ids.insert(user_turn_id_hash(u.record));
                }
            }
        }
    }

    if let Some(parent) = ledger_index_path().parent() {
        fs::create_dir_all(parent).await?;
    }

    let ids_body = if ids.is_empty() {
        String::new()
    } else {
        let mut s = ids.iter().cloned().collect::<Vec<_>>().join("\n");
        s.push('\n');
        s
    };
    let tail_start = content_order.len().saturating_sub(CONTENT_WINDOW);
    let content_tail: Vec<String> = content_order[tail_start..].to_vec();
    let content_body = if content_tail.is_empty() {
        String::new()
    } else {
        let mut s = content_tail.join("\n");
        s.push('\n');
        s
    };

    let ids_for_lock = ids.clone();
    let tail_for_lock = content_tail.clone();
    let report_ids = ids.len();
    let report_content = content_tail.len();

    with_lock("ledger-index", || async move {
        let idx = ledger_index_path();
        let cidx = ledger_content_index_path();
        let idx_tmp = idx.with_extension("idx.tmp");
        let cidx_tmp = cidx.with_extension("content.idx.tmp");
        fs::write(&idx_tmp, ids_body.as_bytes()).await?;
        fs::rename(&idx_tmp, &idx).await?;
        fs::write(&cidx_tmp, content_body.as_bytes()).await?;
        fs::rename(&cidx_tmp, &cidx).await?;

        let snap = IndexSnapshot {
            ids: ids_for_lock,
            content: tail_for_lock.iter().cloned().collect(),
            content_order: tail_for_lock,
            home: ledger_home(),
        };
        let mut guard = CACHE.lock().await;
        *guard = Some(snap);
        Ok(())
    })
    .await?;

    Ok(RebuildReport {
        ids: report_ids,
        content: report_content,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct RebuildReport {
    pub ids: usize,
    pub content: usize,
}

async fn load_hashes(p: &Path) -> Result<HashSet<String>> {
    let lines = load_hashes_array(p).await?;
    Ok(lines.into_iter().collect())
}

async fn load_hashes_array(p: &Path) -> Result<Vec<String>> {
    match fs::File::open(p).await {
        Ok(mut f) => {
            let mut s = String::new();
            f.read_to_string(&mut s).await?;
            Ok(s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

async fn append_lines(p: &Path, hashes: &[String]) -> Result<()> {
    let mut payload = String::new();
    for h in hashes {
        payload.push_str(h);
        payload.push('\n');
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .await?;
    f.write_all(payload.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

// Avoid the "unused" warning on IndexSnapshot::empty until ergonomic
// callers pull it in.
#[allow(dead_code)]
fn _empty_snapshot_used() -> IndexSnapshot {
    IndexSnapshot::empty(std::path::PathBuf::new())
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

    #[tokio::test]
    async fn appends_id_hashes_and_reloads() {
        let (_dir, _g) = fresh_home().await;
        let h1 = "deadbeefcafebabe".to_string();
        let h2 = "0123456789abcdef".to_string();
        append_hashes(&[h1.clone(), h2.clone()], &[]).await.unwrap();

        // Drop cache so we read from disk.
        invalidate_index_cache().await;
        let snap = load_index().await.unwrap();
        assert!(snap.ids.contains(&h1));
        assert!(snap.ids.contains(&h2));
    }

    #[tokio::test]
    async fn rebuild_picks_up_existing_lines() {
        let (_dir, _g) = fresh_home().await;

        // Write a tiny ledger by hand: one turn, one compaction.
        let ledger = ledger_path();
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let turn = LedgerLine::turn(json!({
            "source": "claude",
            "sessionId": "s1",
            "messageId": "m1",
            "ts": "2026-01-01T00:00:00Z",
            "model": "claude-sonnet-4-6",
            "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheCreate5m": 0, "cacheCreate1h": 0},
            "toolCalls": []
        }));
        let mut body = serde_json::to_string(&turn).unwrap();
        body.push('\n');
        std::fs::write(&ledger, body).unwrap();

        let report = rebuild_index().await.unwrap();
        assert_eq!(report.ids, 1);
        assert_eq!(report.content, 1);

        let snap = load_index().await.unwrap();
        assert_eq!(snap.ids.len(), 1);
    }

    #[tokio::test]
    async fn cache_invalidates_on_home_change() {
        let (dir1, _g) = fresh_home().await;
        let h = "aaaaaaaaaaaaaaaa".to_string();
        append_hashes(&[h.clone()], &[]).await.unwrap();

        let _ = dir1; // keep first home alive

        let dir2 = tempdir().unwrap();
        std::env::set_var("RELAYBURN_HOME", dir2.path());
        let snap = load_index().await.unwrap();
        assert!(!snap.ids.contains(&h), "cache should not survive a home swap");
    }
}
