use serde_json::json;

use super::{codex_relationship_key, subagent_notification_status};
use crate::reader::types::{
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, ToolResultStatus,
};

fn relationship(
    source: RelationshipSourceKind,
    relationship_type: RelationshipType,
) -> SessionRelationshipRecord {
    SessionRelationshipRecord {
        v: 1,
        source,
        session_id: "sess".to_string(),
        related_session_id: None,
        relationship_type,
        ts: Some("2026-01-01T00:00:00Z".to_string()),
        source_session_id: Some("src".to_string()),
        source_version: Some("1.0".to_string()),
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: Some("worker".to_string()),
        description: Some("desc".to_string()),
    }
}

#[test]
fn notification_status_prefers_success_bool() {
    assert_eq!(
        subagent_notification_status(&json!({"success": true, "status": "failed"})),
        ToolResultStatus::Completed
    );
    assert_eq!(
        subagent_notification_status(&json!({"success": false, "status": "completed"})),
        ToolResultStatus::Errored
    );
}

#[test]
fn notification_status_maps_status_strings_case_insensitively() {
    for (status, expected) in [
        ("errored", ToolResultStatus::Errored),
        ("FAILED", ToolResultStatus::Errored),
        ("Error", ToolResultStatus::Errored),
        ("cancelled", ToolResultStatus::Cancelled),
        ("Canceled", ToolResultStatus::Cancelled),
        ("completed", ToolResultStatus::Completed),
        ("SUCCESS", ToolResultStatus::Completed),
        ("succeeded", ToolResultStatus::Completed),
        ("running", ToolResultStatus::Completed),
    ] {
        assert_eq!(
            subagent_notification_status(&json!({"status": status})),
            expected,
            "{status}"
        );
    }
}

#[test]
fn notification_status_defaults_to_completed() {
    assert_eq!(
        subagent_notification_status(&json!({})),
        ToolResultStatus::Completed
    );
    assert_eq!(
        subagent_notification_status(&json!({"success": "false", "status": "failed"})),
        ToolResultStatus::Errored
    );
    assert_eq!(
        subagent_notification_status(&json!({"success": null, "status": 3})),
        ToolResultStatus::Completed
    );
}

#[test]
fn relationship_key_labels_every_source_kind() {
    for (source, label) in [
        (RelationshipSourceKind::Codex, "codex"),
        (RelationshipSourceKind::ClaudeCode, "claude-code"),
        (RelationshipSourceKind::Opencode, "opencode"),
        (RelationshipSourceKind::AnthropicApi, "anthropic-api"),
        (RelationshipSourceKind::OpenaiApi, "openai-api"),
        (RelationshipSourceKind::GeminiApi, "gemini-api"),
        (RelationshipSourceKind::SpawnEnv, "spawn-env"),
        (RelationshipSourceKind::NativeClaude, "native-claude"),
        (RelationshipSourceKind::NativeOpencode, "native-opencode"),
    ] {
        assert_eq!(
            codex_relationship_key(&relationship(source, RelationshipType::Root)),
            format!("{label}|sess|root|||")
        );
    }
}

#[test]
fn relationship_key_labels_every_relationship_type() {
    for (rel, label) in [
        (RelationshipType::Root, "root"),
        (RelationshipType::Continuation, "continuation"),
        (RelationshipType::Fork, "fork"),
        (RelationshipType::Subagent, "subagent"),
    ] {
        assert_eq!(
            codex_relationship_key(&relationship(RelationshipSourceKind::Codex, rel)),
            format!("codex|sess|{label}|||")
        );
    }
}

#[test]
fn relationship_key_includes_identity_fields_and_ignores_metadata() {
    let mut row = relationship(RelationshipSourceKind::Codex, RelationshipType::Subagent);
    row.related_session_id = Some("parent".to_string());
    row.agent_id = Some("agent-1".to_string());
    row.parent_tool_use_id = Some("call-9".to_string());
    assert_eq!(
        codex_relationship_key(&row),
        "codex|sess|subagent|parent|agent-1|call-9"
    );
    row.ts = None;
    row.description = None;
    assert_eq!(
        codex_relationship_key(&row),
        "codex|sess|subagent|parent|agent-1|call-9"
    );
}
