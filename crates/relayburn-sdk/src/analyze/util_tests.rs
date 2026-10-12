use serde_json::json;

use super::stringify_tool_result;

#[test]
fn stringify_passes_strings_through_and_blanks_null() {
    assert_eq!(stringify_tool_result(&json!("hello")), "hello");
    assert_eq!(stringify_tool_result(&json!(null)), "");
}

#[test]
fn stringify_serializes_non_array_scalars_and_objects() {
    assert_eq!(stringify_tool_result(&json!(42)), "42");
    assert_eq!(stringify_tool_result(&json!(true)), "true");
    assert_eq!(stringify_tool_result(&json!({"a": 1})), r#"{"a":1}"#);
}

#[test]
fn stringify_joins_array_blocks_with_newlines() {
    let content = json!([
        {"type": "text", "text": "line one"},
        "bare string",
        {"type": "image", "source": "x"},
        {"type": "text"},
        {"type": "text", "text": 7},
        [1, 2],
        3,
        false,
        null,
        {"type": "text", "text": "last"}
    ]);
    assert_eq!(
        stringify_tool_result(&content),
        [
            "line one",
            "bare string",
            r#"{"type":"image","source":"x"}"#,
            r#"{"type":"text"}"#,
            r#"{"type":"text","text":7}"#,
            "[1,2]",
            "last",
        ]
        .join("\n")
    );
}

#[test]
fn stringify_returns_empty_for_empty_or_scalar_only_arrays() {
    assert_eq!(stringify_tool_result(&json!([])), "");
    assert_eq!(stringify_tool_result(&json!([1, true, null])), "");
}

#[test]
fn stringify_takes_text_only_from_text_typed_blocks() {
    assert_eq!(
        stringify_tool_result(&json!([{"type": "note", "text": "hi"}])),
        r#"{"type":"note","text":"hi"}"#
    );
}
