use serde_json::{json, Value};

use super::notification_status;
use crate::reader::types::ToolResultStatus;

fn status(payload: Value) -> ToolResultStatus {
    notification_status(Some(&payload))
}

#[test]
fn notification_status_prefers_success_bool() {
    assert_eq!(
        status(json!({"success": true, "status": "failed"})),
        ToolResultStatus::Completed
    );
    assert_eq!(
        status(json!({"success": false, "status": "completed"})),
        ToolResultStatus::Errored
    );
}

#[test]
fn notification_status_maps_status_strings_case_insensitively() {
    for (raw, expected) in [
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
        assert_eq!(status(json!({ "status": raw })), expected, "{raw}");
    }
}

#[test]
fn notification_status_defaults_to_completed() {
    assert_eq!(notification_status(None), ToolResultStatus::Completed);
    assert_eq!(status(json!({})), ToolResultStatus::Completed);
    assert_eq!(
        status(json!({"success": "false", "status": "failed"})),
        ToolResultStatus::Errored
    );
    assert_eq!(
        status(json!({"success": null, "status": 3})),
        ToolResultStatus::Completed
    );
}
