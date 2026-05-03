//! User-turn block helpers — Rust port of `packages/reader/src/userTurn.ts`.
//!
//! The TS module exposes both a heuristic counter and a cl100k-backed counter
//! (via `@dqbd/tiktoken`). This port covers the heuristic path; the cl100k
//! tokenizer hookup is deferred — when a parser ports over it can pass any
//! `UserTurnTokenCounter` impl that satisfies the trait.

use serde_json::Value;

use crate::types::{UserTurnBlock, UserTurnBlockKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTurnTokenizer {
    Heuristic,
    Cl100k,
}

pub trait UserTurnTokenCounter {
    fn tokenizer(&self) -> UserTurnTokenizer;
    fn count(&self, content: &Value, byte_len: u64) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCounter;

impl UserTurnTokenCounter for HeuristicCounter {
    fn tokenizer(&self) -> UserTurnTokenizer {
        UserTurnTokenizer::Heuristic
    }
    fn count(&self, _content: &Value, byte_len: u64) -> u64 {
        bytes_to_approx_tokens(byte_len)
    }
}

pub fn make_text_block(text: &str, counter: &dyn UserTurnTokenCounter) -> UserTurnBlock {
    let byte_len = text.len() as u64;
    let approx = counter.count(&Value::String(text.to_string()), byte_len);
    UserTurnBlock {
        kind: UserTurnBlockKind::Text,
        tool_use_id: None,
        byte_len,
        approx_tokens: approx,
        is_error: None,
    }
}

pub fn make_tool_result_block(
    tool_use_id: &str,
    content: &Value,
    is_error: Option<bool>,
    counter: &dyn UserTurnTokenCounter,
) -> UserTurnBlock {
    let byte_len = measure_content_bytes(content);
    let approx = counter.count(content, byte_len);
    UserTurnBlock {
        kind: UserTurnBlockKind::ToolResult,
        tool_use_id: Some(tool_use_id.to_string()),
        byte_len,
        approx_tokens: approx,
        // TS only sets is_error when it's true; preserve that wire shape.
        is_error: if is_error == Some(true) {
            Some(true)
        } else {
            None
        },
    }
}

pub fn measure_content_bytes(content: &Value) -> u64 {
    stringify_measured_content(content).len() as u64
}

pub fn stringify_measured_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        // Mirror TS `JSON.stringify`. `serde_json::to_string` emits the same
        // bytes for the shapes we care about (objects, arrays, numbers,
        // booleans).
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub fn bytes_to_approx_tokens(byte_len: u64) -> u64 {
    if byte_len == 0 {
        return 0;
    }
    byte_len.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heuristic_text_block_counts_bytes_div_4_ceil() {
        let block = make_text_block("hello", &HeuristicCounter);
        assert_eq!(block.byte_len, 5);
        assert_eq!(block.approx_tokens, 2); // ceil(5/4) = 2
        assert_eq!(block.kind, UserTurnBlockKind::Text);
    }

    #[test]
    fn tool_result_block_with_string_content() {
        let block =
            make_tool_result_block("tool_1", &json!("hello world"), None, &HeuristicCounter);
        assert_eq!(block.kind, UserTurnBlockKind::ToolResult);
        assert_eq!(block.tool_use_id.as_deref(), Some("tool_1"));
        assert_eq!(block.byte_len, "hello world".len() as u64);
        assert!(block.is_error.is_none());
    }

    #[test]
    fn tool_result_block_with_structured_content_uses_json_stringify() {
        let content = json!({"a": 1, "b": "two"});
        let block = make_tool_result_block("t", &content, Some(false), &HeuristicCounter);
        let expected = serde_json::to_string(&content).unwrap();
        assert_eq!(block.byte_len, expected.len() as u64);
        // is_error: Some(false) is treated as missing on the wire.
        assert!(block.is_error.is_none());
    }

    #[test]
    fn tool_result_block_preserves_is_error_true() {
        let block = make_tool_result_block("t", &json!("err"), Some(true), &HeuristicCounter);
        assert_eq!(block.is_error, Some(true));
    }

    #[test]
    fn measure_null_is_zero() {
        assert_eq!(measure_content_bytes(&Value::Null), 0);
    }

    #[test]
    fn bytes_to_approx_tokens_zero_for_empty() {
        assert_eq!(bytes_to_approx_tokens(0), 0);
        assert_eq!(bytes_to_approx_tokens(1), 1);
        assert_eq!(bytes_to_approx_tokens(4), 1);
        assert_eq!(bytes_to_approx_tokens(5), 2);
    }
}
