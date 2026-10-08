//! OpenCode's tool vocabulary: lowercase tool names whose inputs spell
//! paths `filePath`.

use serde_json::Value;

/// The input field that names what a tool acted on.
pub(super) fn pick_target(name: &str, input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let field = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let path = || {
        field("filePath")
            .or_else(|| field("file_path"))
            .or_else(|| field("path"))
    };
    match name {
        "read" | "write" | "edit" => path(),
        "bash" => field("command"),
        "grep" | "glob" => field("pattern"),
        "webfetch" => field("url"),
        "task" => field("subagent_type")
            .or_else(|| field("description"))
            .or_else(|| field("prompt")),
        _ => path().or_else(|| field("url")).or_else(|| field("command")),
    }
}

/// Tools whose target is a file the turn touched.
pub(super) fn is_file_tool(name: &str) -> bool {
    matches!(name, "read" | "write" | "edit")
}

/// The skill a `skill` call loaded.
pub(super) fn skill_name(name: &str, input: &Value) -> Option<String> {
    if name != "skill" {
        return None;
    }
    let obj = input.as_object()?;
    ["skill", "name", "skill_name"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_follow_the_opencode_field_names() {
        assert_eq!(
            pick_target("read", &json!({"filePath": "/a"})).as_deref(),
            Some("/a")
        );
        assert_eq!(
            pick_target("bash", &json!({"command": "ls"})).as_deref(),
            Some("ls")
        );
        assert_eq!(
            pick_target("glob", &json!({"pattern": "*.rs"})).as_deref(),
            Some("*.rs")
        );
        assert_eq!(
            pick_target("webfetch", &json!({"url": "u"})).as_deref(),
            Some("u")
        );
        assert_eq!(
            pick_target("task", &json!({"prompt": "p"})).as_deref(),
            Some("p")
        );
        assert_eq!(
            pick_target("mcp_x", &json!({"url": "u"})).as_deref(),
            Some("u")
        );
        assert_eq!(pick_target("read", &json!("not an object")), None);
    }

    #[test]
    fn a_skill_call_names_its_skill() {
        assert_eq!(
            skill_name("skill", &json!({"name": "ship-pr"})).as_deref(),
            Some("ship-pr")
        );
        assert_eq!(skill_name("bash", &json!({"name": "ship-pr"})), None);
        assert!(is_file_tool("edit") && !is_file_tool("bash"));
    }
}
