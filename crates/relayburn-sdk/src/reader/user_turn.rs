//! User-turn block helpers — Rust port of `packages/reader/src/userTurn.ts`.
//!
//! Construction lives on the type itself: [`UserTurnBlock::text`] and
//! [`UserTurnBlock::tool_result`] take any [`TokenCounter`]. The default
//! [`HeuristicCounter`] reproduces the bytes/4 estimate the TS port falls
//! back to; the cl100k tokenizer hookup is deferred until a parser actually
//! needs it (no `tiktoken` crate in the dep tree yet — when it lands, plug
//! in a counter that satisfies the trait).

use serde_json::Value;

use crate::reader::types::{UserTurnBlock, UserTurnBlockKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTurnTokenizer {
    Heuristic,
    Cl100k,
}

pub trait TokenCounter {
    fn tokenizer(&self) -> UserTurnTokenizer;
    fn count(&self, content: &Value, byte_len: u64) -> u64;
}

/// Bytes/4 ceiling estimate. Cheap, no dependencies, good enough for
/// proportional allocation across tool calls within one user turn.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn tokenizer(&self) -> UserTurnTokenizer {
        UserTurnTokenizer::Heuristic
    }
    fn count(&self, _content: &Value, byte_len: u64) -> u64 {
        bytes_to_approx_tokens(byte_len)
    }
}

impl UserTurnBlock {
    /// Build a `text` block — plain user input or a harness-injected text
    /// block. `byteLen` is the UTF-8 byte length of `text`; `approxTokens`
    /// comes from the supplied counter.
    pub fn text<C: TokenCounter + ?Sized>(text: &str, counter: &C) -> Self {
        let byte_len = text.len() as u64;
        let approx = counter.count(&Value::String(text.to_string()), byte_len);
        Self {
            kind: UserTurnBlockKind::Text,
            tool_use_id: None,
            byte_len,
            approx_tokens: approx,
            is_error: None,
        }
    }

    /// Build a `tool_result` block. `byte_len` matches how the content would
    /// be serialized into the request body (UTF-8 for plain strings,
    /// `JSON.stringify`'d for structured content). Following the TS shape,
    /// `is_error` is only emitted on the wire when it's `Some(true)`.
    pub fn tool_result<C: TokenCounter + ?Sized>(
        tool_use_id: impl Into<String>,
        content: &Value,
        is_error: Option<bool>,
        counter: &C,
    ) -> Self {
        let byte_len = measure_content_bytes(content);
        let approx = counter.count(content, byte_len);
        Self {
            kind: UserTurnBlockKind::ToolResult,
            tool_use_id: Some(tool_use_id.into()),
            byte_len,
            approx_tokens: approx,
            is_error: if is_error == Some(true) {
                Some(true)
            } else {
                None
            },
        }
    }
}

pub fn measure_content_bytes(content: &Value) -> u64 {
    stringify_measured_content(content).len() as u64
}

pub fn stringify_measured_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        // Mirrors TS `JSON.stringify`. `serde_json::to_string` emits the same
        // bytes for the shapes we care about (objects, arrays, numbers,
        // booleans).
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub fn bytes_to_approx_tokens(byte_len: u64) -> u64 {
    if byte_len == 0 {
        0
    } else {
        byte_len.div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heuristic_text_block_counts_bytes_div_4_ceil() {
        let block = UserTurnBlock::text("hello", &HeuristicCounter);
        assert_eq!(block.byte_len, 5);
        assert_eq!(block.approx_tokens, 2); // ceil(5/4) = 2
        assert_eq!(block.kind, UserTurnBlockKind::Text);
    }

    #[test]
    fn tool_result_block_with_string_content() {
        let block =
            UserTurnBlock::tool_result("tool_1", &json!("hello world"), None, &HeuristicCounter);
        assert_eq!(block.kind, UserTurnBlockKind::ToolResult);
        assert_eq!(block.tool_use_id.as_deref(), Some("tool_1"));
        assert_eq!(block.byte_len, "hello world".len() as u64);
        assert!(block.is_error.is_none());
    }

    #[test]
    fn tool_result_block_with_structured_content_uses_json_stringify() {
        let content = json!({"a": 1, "b": "two"});
        let block = UserTurnBlock::tool_result("t", &content, Some(false), &HeuristicCounter);
        let expected = serde_json::to_string(&content).unwrap();
        assert_eq!(block.byte_len, expected.len() as u64);
        assert!(block.is_error.is_none());
    }

    #[test]
    fn tool_result_block_preserves_is_error_true() {
        let block = UserTurnBlock::tool_result("t", &json!("err"), Some(true), &HeuristicCounter);
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

    #[test]
    fn token_counter_dispatches_via_generics() {
        // Use a custom counter to verify dispatch is generic, not virtual.
        struct Constant(u64);
        impl TokenCounter for Constant {
            fn tokenizer(&self) -> UserTurnTokenizer {
                UserTurnTokenizer::Heuristic
            }
            fn count(&self, _content: &Value, _byte_len: u64) -> u64 {
                self.0
            }
        }
        let block = UserTurnBlock::text("anything", &Constant(7));
        assert_eq!(block.approx_tokens, 7);
    }
}
