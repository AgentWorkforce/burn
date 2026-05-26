//! Discovery and pairing of Claude Code Task subagent sidecar transcripts.
//!
//! Claude Code's `Task` tool spawns subagents whose transcripts land in
//! sidecar `.jsonl` files at:
//!
//! ```text
//! ~/.claude/projects/<slug>/<sessionId>/subagents/agent-<agentId>.jsonl
//! ```
//!
//! Each sidecar may have a companion `agent-<agentId>.meta.json` carrying
//! the spawn-time `agentType`, `description`, and the parent `toolUseId`.
//! The parent session's tool_result row for the dispatching Task tool_use
//! carries `toolUseResult.agentId` referencing the sidecar's filename.
//!
//! This module is deliberately a separate, lazy entry point: walking the
//! `subagents/` directory is opt-in, so default ingest of a session WITHOUT
//! subagent dispatches does not stat anything inside `subagents/`. Callers
//! invoke [`discover_subagents`] only when something downstream wants the
//! sidecar contents.
//!
//! ## Orphan bucket
//!
//! Sidecars whose `agentId` does not match any `toolUseResult.agentId` in
//! the main transcript become unattached. We surface them with
//! [`SubagentTranscript::paired_tool_use_id`] left `None` — the presenter
//! layer labels these as the `UnattachedGroup`. Slash-command synthetic
//! dispatches are an expected source of orphans and are NOT errors.
//!
//! See AgentWorkforce/burn#435.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Parsed contents of one `agent-<id>.jsonl` sidecar plus its companion
/// metadata.
///
/// `records` carries the raw JSONL rows (one `serde_json::Value` per line)
/// so the caller can choose how to ingest them — turning each record into
/// a [`crate::reader::types::TurnRecord`] requires re-running the main
/// parse pipeline, which the ingest path already does file-by-file. The
/// raw shape keeps this module's surface narrow.
#[derive(Debug, Clone)]
pub struct SubagentTranscript {
    /// The `<agentId>` portion of the `agent-<agentId>.jsonl` filename.
    /// Matches the parent session's `toolUseResult.agentId` for paired
    /// sidecars and the in-line `agentId` field carried on every record
    /// inside the sidecar.
    pub agent_id: String,
    /// `agentType` field from the companion `agent-<id>.meta.json`, if
    /// present. Surfaced as a span attribute by the presenter layer.
    pub agent_type: Option<String>,
    /// `description` field from the companion `agent-<id>.meta.json`, if
    /// present. Often a one-line label for the dispatched task.
    pub description: Option<String>,
    /// `toolUseId` field from the companion `agent-<id>.meta.json`, if
    /// present. Used as a fallback pairing key when the parent transcript
    /// does not carry a matching `toolUseResult.agentId`.
    pub meta_tool_use_id: Option<String>,
    /// Raw JSONL rows from the sidecar file, one `serde_json::Value` per
    /// line. Empty when the file existed but had no parseable lines.
    pub records: Vec<Value>,
    /// `tool_use.id` on the parent transcript's Task dispatch whose
    /// matching tool_result carries `toolUseResult.agentId == self.agent_id`.
    /// `None` for orphans — those should be surfaced as `UnattachedGroup`
    /// by the presenter layer.
    pub paired_tool_use_id: Option<String>,
    /// Resolved absolute path to the sidecar file on disk. Useful for
    /// downstream tooling that wants to re-open the file (e.g. for
    /// incremental tail-follow) without re-deriving the path.
    pub source_path: PathBuf,
}

/// Walk `<session_dir>/<session_id>/subagents/agent-*.jsonl` and return
/// one [`SubagentTranscript`] per sidecar file. Empty when the directory
/// does not exist, which is the common case — most sessions never spawn a
/// subagent.
///
/// The companion `agent-<id>.meta.json` is read opportunistically:
/// missing / unparseable metadata leaves `agent_type` etc. as `None`
/// rather than causing the discovery to fail.
///
/// **This function does NOT pair against a parent transcript.** Callers
/// must run [`pair_to_main`] (or perform their own pairing) before
/// trusting `paired_tool_use_id` — discovery leaves it `None` on every
/// transcript by default.
///
/// ## Laziness
///
/// We `metadata()`-check the subagents directory first and bail before
/// any `read_dir` call when it does not exist. The intent is that the
/// ingest path can call this on every session and pay no cost for the
/// (overwhelmingly common) case of no subagent dispatches.
pub fn discover_subagents(session_dir: &Path, session_id: &str) -> Vec<SubagentTranscript> {
    let sub_dir = session_dir.join(session_id).join("subagents");
    // Lazy guard: stat once. If the directory does not exist this is the
    // hot path — exit before `read_dir`.
    match fs::metadata(&sub_dir) {
        Ok(m) if m.is_dir() => {}
        _ => return Vec::new(),
    }
    let entries = match fs::read_dir(&sub_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        // `agent-<id>.jsonl` is the only filename shape we read. The
        // `.meta.json` sibling is opened by `agent_id` below; other files
        // (settings sidecars, future formats) are ignored.
        let agent_id = match parse_agent_filename(name) {
            Some(id) => id,
            None => continue,
        };
        let records = read_jsonl(&path);
        let meta_path = sub_dir.join(format!("agent-{}.meta.json", agent_id));
        let (agent_type, description, meta_tool_use_id) = read_meta(&meta_path);
        out.push(SubagentTranscript {
            agent_id,
            agent_type,
            description,
            meta_tool_use_id,
            records,
            paired_tool_use_id: None,
            source_path: path,
        });
    }
    // Stable ordering by agent_id keeps presenters and tests deterministic
    // regardless of the underlying filesystem's directory enumeration
    // order.
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// Pair a set of discovered subagent transcripts against a main
/// transcript's raw JSONL records.
///
/// `main` is the list of raw `serde_json::Value` rows from the parent
/// session JSONL. We scan it for `toolUseResult.agentId` (the field
/// Claude Code attaches to the parent's tool_result row that completes
/// the Task dispatch), build an `agentId -> tool_use_id` map, and set
/// [`SubagentTranscript::paired_tool_use_id`] on each transcript whose
/// `agent_id` appears in the map.
///
/// Sidecars whose `agent_id` is absent from the map remain unpaired
/// (`paired_tool_use_id = None`) — the presenter labels those as
/// `UnattachedGroup`. As a fallback, if the meta.json carries
/// `toolUseId` we honor it: that lets slash-command-spawned subagents
/// (which never produce a `toolUseResult.agentId` row) still pair when
/// their meta sidecar names the originating tool_use.
pub fn pair_to_main(main: &[Value], subs: Vec<SubagentTranscript>) -> Vec<SubagentTranscript> {
    let agent_to_tool_use = extract_agent_id_to_tool_use_id(main);
    subs.into_iter()
        .map(|mut t| {
            // First-pass: prefer the explicit toolUseResult.agentId
            // linkage found in the parent transcript.
            if let Some(tu) = agent_to_tool_use.get(&t.agent_id) {
                t.paired_tool_use_id = Some(tu.clone());
            } else if let Some(tu) = &t.meta_tool_use_id {
                // Fallback: meta.json carries the toolUseId directly.
                // Only treat as paired if that tool_use actually exists
                // in the main transcript, so we don't conjure a phantom
                // linkage when the parent never ran the dispatching
                // tool_use (e.g. crash before the assistant row landed).
                if main_contains_tool_use_id(main, tu) {
                    t.paired_tool_use_id = Some(tu.clone());
                }
            }
            t
        })
        .collect()
}

/// Strip `agent-` prefix and `.jsonl` suffix from a sidecar filename.
/// Returns `None` for anything that doesn't match — including the
/// `.meta.json` siblings (we read those separately, keyed by agent_id).
fn parse_agent_filename(name: &str) -> Option<String> {
    let after_prefix = name.strip_prefix("agent-")?;
    let stem = after_prefix.strip_suffix(".jsonl")?;
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Read a JSONL file into one `Value` per line. Unparseable lines are
/// dropped (we match the lenient behavior of the main Claude parser:
/// `serde_json::from_str` failures are skipped, not propagated).
fn read_jsonl(path: &Path) -> Vec<Value> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            out.push(v);
        }
    }
    out
}

/// Read the companion `agent-<id>.meta.json` if present. Returns a
/// 3-tuple `(agentType, description, toolUseId)` — each field is
/// `None` when the file is absent, unparseable, or omits that key.
fn read_meta(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return (None, None, None),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return (None, None, None),
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => return (None, None, None),
    };
    let get = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    (get("agentType"), get("description"), get("toolUseId"))
}

/// Build an `agentId -> tool_use_id` map by scanning main-transcript rows
/// for `toolUseResult.agentId`. The matching `tool_use_id` is taken from
/// the same row's `message.content[].tool_use_id` (the tool_result block
/// always carries it; Claude pairs `toolUseResult.agentId` with the
/// async Task launch's tool_use_id one-for-one).
fn extract_agent_id_to_tool_use_id(main: &[Value]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for row in main {
        let obj = match row.as_object() {
            Some(o) => o,
            None => continue,
        };
        let agent_id = obj
            .get("toolUseResult")
            .and_then(|v| v.as_object())
            .and_then(|tur| tur.get("agentId"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let agent_id = match agent_id {
            Some(s) => s,
            None => continue,
        };
        // Pull the tool_use_id from the matching tool_result block. The
        // row's message.content array always carries exactly one
        // tool_result block for an async Task launch, but we scan
        // defensively in case Claude ever batches.
        let tool_use_id = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter().find_map(|b| {
                    let bo = b.as_object()?;
                    if bo.get("type").and_then(Value::as_str)? != "tool_result" {
                        return None;
                    }
                    bo.get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            });
        if let Some(tu) = tool_use_id {
            // First-write-wins so an unlikely repeat agentId in the same
            // file doesn't quietly retarget a previously-paired sidecar.
            out.entry(agent_id.to_string()).or_insert(tu);
        }
    }
    out
}

/// Walk every `<projects_root>/<slug>/<sessionId>/subagents/` directory
/// reachable from `projects_root`, pair each sidecar against its parent
/// session JSONL, and return `(paired, orphan)` counts.
///
/// This is the count surface `burn summary` calls to populate the
/// `subagents: X paired, Y orphan` line. It is intentionally a one-shot
/// scan rather than an ingest hook — the schema's `subagent_id` column
/// already covers the structural query path; this helper is for the
/// presentation count and we want it lazy.
///
/// `session_filter` scopes the count to a specific session-id set so the
/// summary line reflects the same filter the rest of the report was
/// computed with. `None` means "no filter, count every session reachable
/// from `projects_root`" (preserves the original global-summary
/// behavior); `Some(set)` means "only descend into `<sessionId>/` whose
/// name is in `set`". The filter check happens *before* the
/// `subagents/` `read_dir` so it preserves the laziness contract — a
/// filtered summary never stats sidecar directories for sessions it
/// doesn't care about.
///
/// Laziness contract:
/// - `projects_root` missing → returns `(0, 0)` without scanning anything.
/// - Each `<sessionId>` directory missing a `subagents/` child contributes
///   nothing (no `read_dir` past the missing-stat guard).
/// - Session id not in `session_filter` (when `Some`) → directory is
///   skipped entirely; no `discover_subagents` walk.
/// - Parent JSONL missing → every sidecar under that session id is
///   counted as orphan (we have no main transcript to pair against).
pub fn count_subagents_under(
    projects_root: &Path,
    session_filter: Option<&HashSet<String>>,
) -> SubagentCounts {
    let mut counts = SubagentCounts::default();
    let entries = match fs::read_dir(projects_root) {
        Ok(e) => e,
        Err(_) => return counts,
    };
    for project in entries.flatten() {
        let project_dir = project.path();
        if !project_dir.is_dir() {
            continue;
        }
        let session_entries = match fs::read_dir(&project_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for sess in session_entries.flatten() {
            let sess_path = sess.path();
            let name = match sess_path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Sidecar directory is `<project_dir>/<sessionId>/subagents/`.
            // The session id itself is also the basename of a sibling
            // `<sessionId>.jsonl` file under the project dir. We only
            // run discovery for directory entries (skipping the
            // `<sessionId>.jsonl` siblings) so we don't double-account.
            if !sess_path.is_dir() {
                continue;
            }
            // Filter gate: when the caller pinned the summary to a
            // specific session set, skip every other session id BEFORE
            // descending into its `subagents/` tree. Keeps the lazy
            // walk contract intact for the filtered path too.
            if let Some(filter) = session_filter {
                if !filter.contains(&name) {
                    continue;
                }
            }
            let subs = discover_subagents(&project_dir, &name);
            if subs.is_empty() {
                continue;
            }
            // Lazy load the parent file's raw lines for pairing. If the
            // parent JSONL is missing every sidecar falls through to
            // orphan — that mirrors what the issue describes
            // ("UnattachedGroup ... no paired tool_use in the main
            // transcript").
            let parent_path = project_dir.join(format!("{}.jsonl", name));
            let parent_records = read_jsonl(&parent_path);
            let paired = pair_to_main(&parent_records, subs);
            for t in paired {
                if t.paired_tool_use_id.is_some() {
                    counts.paired += 1;
                } else {
                    counts.orphan += 1;
                }
            }
        }
    }
    counts
}

/// Paired / orphan subagent transcript counts. See [`count_subagents_under`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCounts {
    /// Number of sidecar transcripts whose `agent_id` matched a
    /// `toolUseResult.agentId` in the parent transcript.
    pub paired: u64,
    /// Number of sidecar transcripts that did not pair — surfaced as
    /// the `UnattachedGroup` bucket. Includes slash-command synthetic
    /// dispatches (expected, not an error condition).
    pub orphan: u64,
}

impl SubagentCounts {
    /// `true` when both counts are zero; presenters use this to skip the
    /// summary line entirely on sessions that never spawned a subagent.
    pub fn is_empty(&self) -> bool {
        self.paired == 0 && self.orphan == 0
    }

    /// Total subagent transcripts (paired + orphan).
    pub fn total(&self) -> u64 {
        self.paired + self.orphan
    }
}

/// True when any assistant row in `main` declares a `tool_use` block
/// with the given id. Used as a sanity gate for the meta.json fallback
/// pairing path so a stale meta sidecar can't conjure a phantom
/// `paired_tool_use_id` pointing at a tool_use the parent never ran.
fn main_contains_tool_use_id(main: &[Value], tool_use_id: &str) -> bool {
    for row in main {
        let obj = match row.as_object() {
            Some(o) => o,
            None => continue,
        };
        let blocks = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let blocks = match blocks {
            Some(b) => b,
            None => continue,
        };
        for block in blocks {
            let bo = match block.as_object() {
                Some(o) => o,
                None => continue,
            };
            if bo.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if bo.get("id").and_then(Value::as_str) == Some(tool_use_id) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write_sidecar(dir: &Path, agent_id: &str, lines: &[Value]) {
        fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        for l in lines {
            body.push_str(&serde_json::to_string(l).unwrap());
            body.push('\n');
        }
        fs::write(dir.join(format!("agent-{}.jsonl", agent_id)), body).unwrap();
    }

    fn write_meta(dir: &Path, agent_id: &str, value: &Value) {
        fs::write(
            dir.join(format!("agent-{}.meta.json", agent_id)),
            serde_json::to_string_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn task_dispatch_pair(tool_use_id: &str, agent_id: &str) -> (Value, Value) {
        // Assistant row carrying a Task tool_use the subagent corresponds to.
        let assistant = json!({
            "type": "assistant",
            "uuid": format!("asst-{}", tool_use_id),
            "sessionId": "session-1",
            "message": {
                "id": format!("msg-{}", tool_use_id),
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": tool_use_id, "name": "Task", "input": {}}
                ],
                "usage": {"input_tokens": 10, "output_tokens": 1},
                "stop_reason": "tool_use"
            }
        });
        // User-shaped tool_result row carrying toolUseResult.agentId — this
        // is the linkage field the issue asks us to pair on.
        let tool_result = json!({
            "type": "user",
            "uuid": format!("result-{}", tool_use_id),
            "sessionId": "session-1",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": [{"type": "text", "text": "Async agent launched"}]
                    }
                ]
            },
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": agent_id
            }
        });
        (assistant, tool_result)
    }

    #[test]
    fn discover_returns_empty_when_directory_absent() {
        // Hot path: a session with no subagents must not pay a directory
        // walk. We can only assert the empty return here; the laziness
        // gate is exercised by the dedicated test below.
        let tmp = tempfile::tempdir().unwrap();
        let subs = discover_subagents(tmp.path(), "session-1");
        assert!(subs.is_empty());
    }

    #[test]
    fn discover_is_lazy_when_session_dir_missing() {
        // Per AgentWorkforce/burn#435: discovery must not touch the
        // subagents directory when the session dir does not exist.
        // We use a non-existent root + an arbitrary session id so even
        // the `read_dir` would fail loudly if we reached it; an empty
        // return here means the early `metadata` gate kicked in.
        let nonexistent = std::path::PathBuf::from("/this/path/does/not/exist-burn-435");
        let subs = discover_subagents(&nonexistent, "session-1");
        assert!(subs.is_empty());
    }

    #[test]
    fn discover_reads_jsonl_and_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();
        let sub_dir = session_dir.join("session-1").join("subagents");
        write_sidecar(
            &sub_dir,
            "abc",
            &[json!({"type": "user", "agentId": "abc", "uuid": "u1"})],
        );
        write_meta(
            &sub_dir,
            "abc",
            &json!({
                "agentType": "general-purpose",
                "description": "do the thing",
                "toolUseId": "toolu_x"
            }),
        );
        let subs = discover_subagents(session_dir, "session-1");
        assert_eq!(subs.len(), 1);
        let t = &subs[0];
        assert_eq!(t.agent_id, "abc");
        assert_eq!(t.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(t.description.as_deref(), Some("do the thing"));
        assert_eq!(t.meta_tool_use_id.as_deref(), Some("toolu_x"));
        assert_eq!(t.records.len(), 1);
        assert!(t.paired_tool_use_id.is_none(), "discover does not pair");
    }

    #[test]
    fn discover_handles_meta_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sub_dir = tmp.path().join("session-1").join("subagents");
        write_sidecar(
            &sub_dir,
            "abc",
            &[json!({"type": "user", "agentId": "abc"})],
        );
        let subs = discover_subagents(tmp.path(), "session-1");
        assert_eq!(subs.len(), 1);
        assert!(subs[0].agent_type.is_none());
        assert!(subs[0].description.is_none());
        assert!(subs[0].meta_tool_use_id.is_none());
    }

    #[test]
    fn pair_to_main_attaches_via_tool_use_result_agent_id() {
        // The canonical pairing: parent transcript carries
        // toolUseResult.agentId on the Task dispatch's tool_result row,
        // and the sidecar's filename agentId matches.
        let (assistant, tool_result) = task_dispatch_pair("toolu_a", "agent-aaa");
        let main = vec![assistant, tool_result];
        let subs = vec![SubagentTranscript {
            agent_id: "agent-aaa".to_string(),
            agent_type: None,
            description: None,
            meta_tool_use_id: None,
            records: vec![],
            paired_tool_use_id: None,
            source_path: PathBuf::from("/tmp/agent-aaa.jsonl"),
        }];
        let paired = pair_to_main(&main, subs);
        assert_eq!(paired[0].paired_tool_use_id.as_deref(), Some("toolu_a"));
    }

    #[test]
    fn pair_to_main_leaves_orphans_unpaired() {
        // Slash-command synthetic dispatches NEVER produce a
        // toolUseResult.agentId row in the parent; they are EXPECTED
        // orphans and must surface as UnattachedGroup, not as errors.
        let main: Vec<Value> = Vec::new();
        let subs = vec![SubagentTranscript {
            agent_id: "agent-orphan".to_string(),
            agent_type: Some("slash-skill".to_string()),
            description: None,
            meta_tool_use_id: None,
            records: vec![],
            paired_tool_use_id: None,
            source_path: PathBuf::from("/tmp/agent-orphan.jsonl"),
        }];
        let paired = pair_to_main(&main, subs);
        assert!(paired[0].paired_tool_use_id.is_none());
    }

    #[test]
    fn pair_to_main_uses_meta_tool_use_id_when_parent_lacks_agent_id_link() {
        // Some sidecars (slash-command-spawned) carry toolUseId in
        // meta.json even when the parent never emitted the
        // toolUseResult.agentId row. We pair via meta IF the parent
        // actually ran that tool_use (the assistant row's tool_use.id
        // matches) — otherwise we leave it orphan to avoid conjuring a
        // phantom linkage.
        let assistant = json!({
            "type": "assistant",
            "uuid": "asst-x",
            "sessionId": "session-1",
            "message": {
                "id": "msg-x",
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_meta", "name": "Task", "input": {}}
                ]
            }
        });
        let main = vec![assistant];
        let subs = vec![SubagentTranscript {
            agent_id: "agent-bbb".to_string(),
            agent_type: None,
            description: None,
            meta_tool_use_id: Some("toolu_meta".to_string()),
            records: vec![],
            paired_tool_use_id: None,
            source_path: PathBuf::from("/tmp/agent-bbb.jsonl"),
        }];
        let paired = pair_to_main(&main, subs);
        assert_eq!(paired[0].paired_tool_use_id.as_deref(), Some("toolu_meta"));
    }

    #[test]
    fn pair_to_main_ignores_meta_tool_use_id_when_main_does_not_have_it() {
        // Stale meta sidecar pointing at a tool_use the parent never
        // ran: must NOT be paired — otherwise we'd attach the
        // transcript to a phantom span.
        let main: Vec<Value> = Vec::new();
        let subs = vec![SubagentTranscript {
            agent_id: "agent-ccc".to_string(),
            agent_type: None,
            description: None,
            meta_tool_use_id: Some("toolu_phantom".to_string()),
            records: vec![],
            paired_tool_use_id: None,
            source_path: PathBuf::from("/tmp/agent-ccc.jsonl"),
        }];
        let paired = pair_to_main(&main, subs);
        assert!(paired[0].paired_tool_use_id.is_none());
    }

    #[test]
    fn count_subagents_under_missing_root_returns_zero() {
        // Hot path: ledger sweep against a non-existent projects root
        // must not panic and must not pay any directory walk.
        let nonexistent = std::path::PathBuf::from("/this/path/does/not/exist-burn-435");
        let counts = count_subagents_under(&nonexistent, None);
        assert_eq!(counts, SubagentCounts::default());
        assert!(counts.is_empty());
    }

    #[test]
    fn count_subagents_under_sums_paired_and_orphan_across_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path();
        // Project A — session-a has 1 paired + 1 orphan sidecar.
        let project_a = projects_root.join("project-a");
        let sub_a = project_a.join("session-a").join("subagents");
        write_sidecar(&sub_a, "p1", &[json!({"type": "user"})]);
        write_sidecar(&sub_a, "o1", &[json!({"type": "user"})]);
        let (asst, res) = task_dispatch_pair("toolu_p1", "p1");
        let parent_a_body = format!(
            "{}\n{}\n",
            serde_json::to_string(&asst).unwrap(),
            serde_json::to_string(&res).unwrap()
        );
        fs::write(project_a.join("session-a.jsonl"), parent_a_body).unwrap();
        // Project B — session-b has 1 paired sidecar.
        let project_b = projects_root.join("project-b");
        let sub_b = project_b.join("session-b").join("subagents");
        write_sidecar(&sub_b, "p2", &[json!({"type": "user"})]);
        let (asst2, res2) = task_dispatch_pair("toolu_p2", "p2");
        let parent_b_body = format!(
            "{}\n{}\n",
            serde_json::to_string(&asst2).unwrap(),
            serde_json::to_string(&res2).unwrap()
        );
        fs::write(project_b.join("session-b.jsonl"), parent_b_body).unwrap();
        // Project C — session-c has 1 orphan with no parent JSONL at
        // all (the parent file got pruned but the sidecar tree
        // survived); orphans accumulate without crashing.
        let project_c = projects_root.join("project-c");
        let sub_c = project_c.join("session-c").join("subagents");
        write_sidecar(&sub_c, "lonely", &[json!({"type": "user"})]);

        let counts = count_subagents_under(projects_root, None);
        assert_eq!(counts.paired, 2, "got: {:?}", counts);
        assert_eq!(counts.orphan, 2, "got: {:?}", counts);
        assert_eq!(counts.total(), 4);
        assert!(!counts.is_empty());
    }

    /// Helper: build a `projects_root` with two sessions across two
    /// projects, each holding subagent sidecars. Returns
    /// `(tmp, projects_root)` so callers can run filtered counts.
    fn two_session_subagent_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().to_path_buf();
        // Project A — session-a has 1 paired + 1 orphan sidecar.
        let project_a = projects_root.join("project-a");
        let sub_a = project_a.join("session-a").join("subagents");
        write_sidecar(&sub_a, "p1", &[json!({"type": "user"})]);
        write_sidecar(&sub_a, "o1", &[json!({"type": "user"})]);
        let (asst, res) = task_dispatch_pair("toolu_p1", "p1");
        let parent_a_body = format!(
            "{}\n{}\n",
            serde_json::to_string(&asst).unwrap(),
            serde_json::to_string(&res).unwrap()
        );
        fs::write(project_a.join("session-a.jsonl"), parent_a_body).unwrap();
        // Project B — session-b has 1 paired sidecar.
        let project_b = projects_root.join("project-b");
        let sub_b = project_b.join("session-b").join("subagents");
        write_sidecar(&sub_b, "p2", &[json!({"type": "user"})]);
        let (asst2, res2) = task_dispatch_pair("toolu_p2", "p2");
        let parent_b_body = format!(
            "{}\n{}\n",
            serde_json::to_string(&asst2).unwrap(),
            serde_json::to_string(&res2).unwrap()
        );
        fs::write(project_b.join("session-b.jsonl"), parent_b_body).unwrap();
        (tmp, projects_root)
    }

    #[test]
    fn count_subagents_under_filters_to_named_session_only() {
        // `burn summary --session session-a` must only count subagents
        // under that session id — sidecars under sibling session ids
        // (here `session-b`) must not contribute to the line.
        let (_tmp, projects_root) = two_session_subagent_fixture();
        let mut filter = HashSet::new();
        filter.insert("session-a".to_string());
        let counts = count_subagents_under(&projects_root, Some(&filter));
        assert_eq!(counts.paired, 1, "only session-a's pair counts");
        assert_eq!(counts.orphan, 1, "only session-a's orphan counts");
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn count_subagents_under_no_filter_matches_pre_filter_behavior() {
        // Sanity guard for the existing global-summary code path: an
        // unfiltered call against the same fixture must still observe
        // every reachable sidecar. This pins the "filters never break
        // the un-filtered call" contract.
        let (_tmp, projects_root) = two_session_subagent_fixture();
        let counts = count_subagents_under(&projects_root, None);
        assert_eq!(counts.paired, 2);
        assert_eq!(counts.orphan, 1);
        assert_eq!(counts.total(), 3);
    }

    #[test]
    fn count_subagents_under_filter_with_unknown_session_returns_zero() {
        // Filter contains session ids that are not on disk (e.g. the
        // ledger has filtered rows for a session whose sidecar tree
        // was pruned). The walk must skip every existing session,
        // return zeros, and not panic.
        let (_tmp, projects_root) = two_session_subagent_fixture();
        let mut filter = HashSet::new();
        filter.insert("session-does-not-exist".to_string());
        let counts = count_subagents_under(&projects_root, Some(&filter));
        assert_eq!(counts, SubagentCounts::default());
        assert!(counts.is_empty());
    }

    #[test]
    fn count_subagents_under_empty_filter_returns_zero() {
        // An empty (but `Some`) filter means "no session is in scope"
        // — counts must come back zero rather than falling through to
        // the global walk.
        let (_tmp, projects_root) = two_session_subagent_fixture();
        let filter = HashSet::new();
        let counts = count_subagents_under(&projects_root, Some(&filter));
        assert_eq!(counts, SubagentCounts::default());
    }

    #[test]
    fn discover_pair_end_to_end_two_paired_one_orphan() {
        // Acceptance fixture from the issue:
        //   "session with two paired Task dispatches and one orphan
        //    subagent. Pairing works; orphan surfaces as UnattachedGroup."
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();
        let sub_dir = session_dir.join("session-1").join("subagents");
        for id in ["aaa", "bbb", "ccc"] {
            write_sidecar(
                &sub_dir,
                id,
                &[json!({"type": "user", "agentId": id, "uuid": format!("u-{}", id)})],
            );
        }
        // Meta only for the paired ones to prove meta is optional.
        write_meta(&sub_dir, "aaa", &json!({"agentType": "general-purpose"}));
        write_meta(&sub_dir, "bbb", &json!({"agentType": "code-reviewer"}));

        let (a_asst, a_result) = task_dispatch_pair("toolu_a", "aaa");
        let (b_asst, b_result) = task_dispatch_pair("toolu_b", "bbb");
        // "ccc" has no matching tool_result.agentId in the parent — it
        // is the expected orphan.
        let main = vec![a_asst, a_result, b_asst, b_result];

        let subs = discover_subagents(session_dir, "session-1");
        assert_eq!(subs.len(), 3);
        let paired = pair_to_main(&main, subs);

        let by_id: std::collections::HashMap<&str, &SubagentTranscript> =
            paired.iter().map(|t| (t.agent_id.as_str(), t)).collect();
        assert_eq!(by_id["aaa"].paired_tool_use_id.as_deref(), Some("toolu_a"));
        assert_eq!(by_id["aaa"].agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(by_id["bbb"].paired_tool_use_id.as_deref(), Some("toolu_b"));
        assert_eq!(by_id["bbb"].agent_type.as_deref(), Some("code-reviewer"));
        assert!(
            by_id["ccc"].paired_tool_use_id.is_none(),
            "third sidecar is the expected UnattachedGroup orphan"
        );

        // Counts the presenter would report: 2 paired, 1 orphan.
        let paired_count = paired
            .iter()
            .filter(|t| t.paired_tool_use_id.is_some())
            .count();
        let orphan_count = paired.len() - paired_count;
        assert_eq!(paired_count, 2);
        assert_eq!(orphan_count, 1);
    }
}
