//! Tool-call attributes burn derives from a call's name and arguments: its
//! target, the content hashes an edit carries, and the files a turn touched.

use std::collections::HashSet;

use serde_json::Value;

use crate::reader::hash::content_hash;
use crate::reader::types::ToolCall;

/// Content hashes of the text an `Edit`/`NotebookEdit` replaces and writes,
/// or a `Write` writes.
pub(super) fn apply_edit_hashes(call: &mut ToolCall, input: &Value) {
    let Some(obj) = input.as_object() else {
        return;
    };
    let hash = |key: &str| obj.get(key).and_then(Value::as_str).map(content_hash);
    match call.name.as_str() {
        "Edit" | "NotebookEdit" => {
            call.edit_pre_hash = hash("old_string");
            call.edit_post_hash = hash("new_string");
        }
        "Write" => call.edit_post_hash = hash("content"),
        _ => {}
    }
}

/// The argument that names what a call acts on.
pub(super) fn pick_target(name: &str, input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => s("file_path"),
        "Bash" => s("command"),
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "Agent" | "Task" => s("subagent_type").or_else(|| s("description")),
        _ => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("url"))
            .or_else(|| s("command")),
    }
}

/// Distinct file targets of the turn's file tools, in call order.
pub(super) fn extract_files_touched(tool_calls: &[ToolCall]) -> Vec<String> {
    let mut seen = HashSet::new();
    tool_calls
        .iter()
        .filter(|tc| matches!(tc.name.as_str(), "Read" | "Edit" | "Write" | "NotebookEdit"))
        .filter_map(|tc| tc.target.clone())
        .filter(|target| seen.insert(target.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, target: Option<&str>) -> ToolCall {
        ToolCall {
            id: format!("{name}-id"),
            name: name.to_string(),
            target: target.map(str::to_string),
            args_hash: String::new(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    #[test]
    fn targets_follow_the_tool_name() {
        let input = json!({"file_path": "/a", "command": "ls", "pattern": "p", "url": "u"});
        assert_eq!(pick_target("Read", &input).as_deref(), Some("/a"));
        assert_eq!(pick_target("Bash", &input).as_deref(), Some("ls"));
        assert_eq!(pick_target("Glob", &input).as_deref(), Some("p"));
        assert_eq!(pick_target("WebFetch", &input).as_deref(), Some("u"));
        let task = json!({"description": "d"});
        assert_eq!(pick_target("Task", &task).as_deref(), Some("d"));
        assert_eq!(
            pick_target("Other", &json!({"path": "/p"})).as_deref(),
            Some("/p")
        );
        assert_eq!(pick_target("Read", &json!("x")), None);
    }

    #[test]
    fn edits_carry_content_hashes() {
        let mut edit = call("Edit", None);
        apply_edit_hashes(&mut edit, &json!({"old_string": "a", "new_string": "b"}));
        assert_eq!(edit.edit_pre_hash, Some(content_hash("a")));
        assert_eq!(edit.edit_post_hash, Some(content_hash("b")));

        let mut write = call("Write", None);
        apply_edit_hashes(&mut write, &json!({"content": "c"}));
        assert_eq!(write.edit_pre_hash, None);
        assert_eq!(write.edit_post_hash, Some(content_hash("c")));

        let mut bash = call("Bash", None);
        apply_edit_hashes(&mut bash, &json!({"command": "ls"}));
        assert_eq!(bash.edit_post_hash, None);
    }

    #[test]
    fn files_touched_are_distinct_file_tool_targets() {
        let calls = [
            call("Read", Some("/a")),
            call("Edit", Some("/a")),
            call("Bash", Some("ls")),
            call("Write", Some("/b")),
            call("Read", None),
        ];
        assert_eq!(extract_files_touched(&calls), ["/a", "/b"]);
    }
}
