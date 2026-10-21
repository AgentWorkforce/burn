//! Codex: burn's records come from the rollout's task lifecycle rather
//! than from relayhistory's request grouping. A burn turn is one task
//! (`task_started` … `task_complete` / `turn_aborted`), its usage is the
//! difference of the cumulative `token_count` snapshots across it, and
//! everything a task derives is emitted only once it commits. Each
//! snapshot that moves the running total inside a task ends one model
//! request: the turn counts them (`request_count`) and each becomes an
//! inference row carrying its own share of the task's spend.

use ai_hist::SessionEvidence;

use crate::source::{Billing, SessionRecords};

mod context;
mod events;
mod records;
mod relationships;
mod snapshots;
mod targets;
mod tasks;

/// Burn's records for one Codex session.
pub(super) fn records(ev: &SessionEvidence, billing: &Billing) -> SessionRecords {
    let stream = events::stream(ev);
    let derived = tasks::Tasks::new(ev, billing).run(&stream);
    let mut relationships = Vec::new();
    if derived.committed {
        relationships = relationships::session_meta(ev);
        relationships.extend(derived.subagents);
    }
    SessionRecords {
        turns: derived.turns,
        content: derived.content,
        compactions: derived.compactions,
        relationships,
        tool_result_events: derived.tool_result_events,
        user_turns: derived.user_turns,
        request_id_lookup: Default::default(),
        inferences: Some(derived.inferences),
        pending: derived.pending,
        copied_history: copied_history(ev),
    }
}

/// Whether relayhistory set aside history this fork copied from its
/// parent: the copy is one `fork_replay_boundary` marker of subkind
/// `copied_history`.
fn copied_history(ev: &SessionEvidence) -> bool {
    ev.markers
        .iter()
        .any(|m| m.kind == "fork_replay_boundary" && m.subkind.as_deref() == Some("copied_history"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A Codex session carrying one marker per `(kind, subkind)`.
    fn with_markers(markers: &[(&str, &str)]) -> SessionEvidence {
        let markers: Vec<_> = markers
            .iter()
            .enumerate()
            .map(|(line, (kind, subkind))| {
                json!({
                    "marker_uid": format!("{line}:marker"), "ts_ms": line, "message_id": null,
                    "parent_id": null, "turn_id": null, "kind": kind, "subkind": subkind,
                    "text": null, "payload": null, "usage_snapshot": null
                })
            })
            .collect();
        serde_json::from_value(json!({
            "session": {
                "source": "codex", "session_id": "sess", "cwd": "/tmp/project",
                "git_branch": null, "first_activity_ms": 0, "last_activity_ms": 0,
                "first_prompt": null, "last_assistant_text": null, "models": [],
                "originator": null, "agent_version": null, "repo_url": null,
                "initial_commit": null, "workspace_roots": [], "project_key": null,
                "project_key_method": null, "raw_path": null, "source_stamp": null,
                "discovery_state": "full", "locations": ["local"]
            },
            "prompts": [], "messages": [], "tool_calls": [], "tool_results": [],
            "file_edits": [], "markers": markers, "relationships": [], "requests": [],
            "usage": null, "user_turns": [], "coverage": [], "loaded": [],
            "include_text": true, "diagnostics": []
        }))
        .unwrap()
    }

    #[test]
    fn only_a_copied_history_boundary_marks_a_copy() {
        let copied = [("fork_replay_boundary", "copied_history")];
        assert!(copied_history(&with_markers(&copied)));
        for markers in [
            vec![],
            vec![("fork_replay_boundary", "session_meta")],
            vec![("compaction_boundary", "copied_history")],
        ] {
            assert!(!copied_history(&with_markers(&markers)), "{markers:?}");
        }
    }
}
