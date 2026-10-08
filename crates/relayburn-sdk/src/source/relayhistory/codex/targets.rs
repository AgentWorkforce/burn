//! The principal argument of a Codex tool call: the command an
//! `exec_command`/`shell` ran, the file a read or write named, the file an
//! `apply_patch` edits.

use serde_json::Value;

/// Target of a `function_call`, from its parsed `arguments`.
pub(super) fn function_call_target(name: &str, args: Option<&Value>) -> Option<String> {
    let args = args?.as_object()?;
    let first = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    };
    match name {
        "exec_command" | "shell" => first(&["cmd", "command"]),
        "read_file" | "write_file" => first(&["path", "file_path"]),
        _ => first(&["path", "file_path", "cmd", "command", "url"]),
    }
}

/// Target of a `custom_tool_call`: the first file an `apply_patch` names
/// in a `*** Update|Add|Delete File: <path>` header.
pub(super) fn custom_tool_target(name: &str, input: &str) -> Option<String> {
    if name != "apply_patch" {
        return None;
    }
    input.lines().find_map(patch_header_path)
}

fn patch_header_path(line: &str) -> Option<String> {
    let line = line.trim_start();
    if !line.starts_with("***") {
        return None;
    }
    let rest = line.trim_start_matches('*').trim_start();
    ["Update", "Add", "Delete"].iter().find_map(|verb| {
        let path = rest
            .strip_prefix(verb)?
            .trim_start()
            .strip_prefix("File:")?
            .trim();
        (!path.is_empty()).then(|| path.to_string())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn exec_targets_the_command() {
        let args = json!({"cmd": "git status", "workdir": "/tmp/project"});
        assert_eq!(
            function_call_target("exec_command", Some(&args)).as_deref(),
            Some("git status")
        );
        let args = json!({"command": "ls"});
        assert_eq!(
            function_call_target("shell", Some(&args)).as_deref(),
            Some("ls")
        );
    }

    #[test]
    fn non_string_command_has_no_target() {
        let args = json!({"command": ["cat", "huge.log"]});
        assert_eq!(function_call_target("shell", Some(&args)), None);
    }

    #[test]
    fn file_tools_target_the_path() {
        let args = json!({"file_path": "/a.rs", "cmd": "x"});
        assert_eq!(
            function_call_target("read_file", Some(&args)).as_deref(),
            Some("/a.rs")
        );
        assert_eq!(
            function_call_target("write_file", Some(&json!({"path": "/b"}))).as_deref(),
            Some("/b")
        );
    }

    #[test]
    fn other_tools_take_the_first_known_key() {
        let args = json!({"url": "https://x", "command": "c"});
        assert_eq!(
            function_call_target("fetch", Some(&args)).as_deref(),
            Some("c")
        );
        assert_eq!(
            function_call_target("fetch", Some(&json!({"url": "https://x"}))).as_deref(),
            Some("https://x")
        );
        assert_eq!(function_call_target("fetch", Some(&json!({}))), None);
        assert_eq!(function_call_target("fetch", Some(&json!("str"))), None);
        assert_eq!(function_call_target("fetch", None), None);
    }

    #[test]
    fn apply_patch_targets_the_first_file_header() {
        let input = "*** Begin Patch\n*** Update File: /tmp/project/README.md\n@@\n+banner\n*** End Patch\n";
        assert_eq!(
            custom_tool_target("apply_patch", input).as_deref(),
            Some("/tmp/project/README.md")
        );
        let input = "*** Begin Patch\n  ***  Delete File:   /gone.txt  \n";
        assert_eq!(
            custom_tool_target("apply_patch", input).as_deref(),
            Some("/gone.txt")
        );
        assert_eq!(
            custom_tool_target("apply_patch", "*** Add File:   \n*** End Patch"),
            None
        );
        assert_eq!(custom_tool_target("other_tool", "*** Add File: /x"), None);
    }
}
