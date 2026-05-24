//! Per-file ingest cursors — Rust port of `packages/ledger/src/cursors.ts`.
//!
//! Cursors live in the `archive_state.upstream_cursors_json` blob in
//! `burn.sqlite`; this module owns the typed schema layered over that string,
//! plus the load / save / diff helpers the orchestration code uses.
//!
//! ## Wire layout
//!
//! `{"files": {"<absolute path>": <cursor>}}`. The cursor variant is tagged
//! by `kind`: `"claude" | "codex" | "opencode" | "opencode-stream"`. Field
//! names are camelCase to match the TS schema so a Rust ingest can pick up
//! cursors a TS ingest wrote, and vice versa, during the migration.

use std::collections::BTreeMap;

use crate::ledger::{Ledger, Result as LedgerResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCursor {
    pub inode: u64,
    pub offset_bytes: u64,
    pub mtime_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCumulative {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCursor {
    pub inode: u64,
    pub offset_bytes: u64,
    pub mtime_ms: i64,
    pub cumulative: CodexCumulative,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cwd: Option<String>,
    #[serde(default)]
    pub turn_contexts: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_turn_slot: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub root_session_emitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_event_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_counters: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_completed_turn: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeCursor {
    pub inode: u64,
    pub mtime_ms: i64,
    pub seen_message_ids: Vec<String>,
}

/// Tagged-union cursor variants. The Codex variant is heap-boxed because it
/// carries a per-turn map and dwarfs the others — keeping the enum payload
/// size sane keeps `Cursors` cheap to clone in the orchestration hot path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FileCursor {
    Claude(ClaudeCursor),
    Codex(Box<CodexCursor>),
    Opencode(OpencodeCursor),
    #[serde(rename = "opencode-stream")]
    OpencodeStream(Value),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CursorsFile {
    #[serde(default)]
    files: Map<String, Value>,
}

/// Cursor map — preserves unknown variants verbatim by keeping each entry as
/// a `serde_json::Value` so a future cursor kind doesn't get silently
/// stripped on round-trip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cursors {
    pub files: BTreeMap<String, Value>,
}

impl Cursors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_typed(&self, key: &str) -> Option<FileCursor> {
        self.files
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    pub fn insert(&mut self, key: String, cursor: FileCursor) {
        let value = serde_json::to_value(&cursor).expect("FileCursor serializes");
        self.files.insert(key, value);
    }

    #[cfg(test)]
    pub fn insert_raw(&mut self, key: String, value: Value) {
        self.files.insert(key, value);
    }
}

/// Load cursors from `Ledger::read_cursors()`. Empty / malformed payloads
/// degrade to an empty map so a corrupt blob doesn't lock out ingest.
pub fn load_cursors(ledger: &Ledger) -> LedgerResult<Cursors> {
    let raw = ledger.read_cursors()?;
    if raw.is_empty() {
        return Ok(Cursors::new());
    }
    let parsed: CursorsFile = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(Cursors::new()),
    };
    Ok(Cursors {
        files: parsed.files.into_iter().collect(),
    })
}

/// Persist `cursors` back to the ledger. Wraps the map in the `{"files": ...}`
/// envelope the TS adapter uses so a `burn` binary that mixes Rust and TS
/// passes (during the migration) sees a consistent on-disk shape.
pub fn save_cursors(ledger: &mut Ledger, cursors: &Cursors) -> LedgerResult<()> {
    let mut map = Map::new();
    for (k, v) in &cursors.files {
        map.insert(k.clone(), v.clone());
    }
    let payload = serde_json::json!({ "files": Value::Object(map) });
    let json = serde_json::to_string(&payload).expect("cursors serializes");
    ledger.write_cursors(&json)
}

/// If `after` differs from `before`, persist `after` in full; otherwise
/// no-op. The early-return spares the write lock when an ingest pass
/// produced no cursor changes — it does NOT compute a per-key diff
/// (despite an earlier name + doc that claimed it did). Per-key delta
/// writes are tracked separately as a perf follow-up; today every
/// non-empty change rewrites the whole cursor map.
pub fn save_cursors_if_changed(
    ledger: &mut Ledger,
    before: &Cursors,
    after: &Cursors,
) -> LedgerResult<()> {
    if before == after {
        return Ok(());
    }
    save_cursors(ledger, after)
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_cursor_roundtrip() {
        let c = FileCursor::Claude(ClaudeCursor {
            inode: 42,
            offset_bytes: 1024,
            mtime_ms: 1_700_000_000_000,
            last_user_text: Some("hello".into()),
        });
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""kind":"claude""#));
        assert!(json.contains(r#""offsetBytes":1024"#));
        assert!(json.contains(r#""lastUserText":"hello""#));
        let back: FileCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn opencode_cursor_camelcase() {
        let c = FileCursor::Opencode(OpencodeCursor {
            inode: 1,
            mtime_ms: 2,
            seen_message_ids: vec!["m1".into(), "m2".into()],
        });
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""kind":"opencode""#));
        assert!(json.contains(r#""seenMessageIds":["m1","m2"]"#));
    }

    #[test]
    fn unknown_variant_preserved_as_raw_value() {
        let mut cursors = Cursors::new();
        let raw = serde_json::json!({"kind":"future-shape","x":1});
        cursors.insert_raw("path/x".into(), raw.clone());
        let json = serde_json::to_string(&serde_json::json!({"files": &cursors.files})).unwrap();
        assert!(json.contains(r#""future-shape""#));
    }
}
