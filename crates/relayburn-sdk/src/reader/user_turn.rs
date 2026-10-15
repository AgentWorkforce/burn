//! User-turn block sizing.

use crate::reader::types::{UserTurnBlock, UserTurnBlockKind};

impl UserTurnBlock {
    /// A `text` block: plain user input or a harness-injected text block,
    /// sized by its UTF-8 byte length.
    pub fn text(text: &str) -> Self {
        let byte_len = text.len() as u64;
        Self {
            kind: UserTurnBlockKind::Text,
            tool_use_id: None,
            byte_len,
            approx_tokens: bytes_to_approx_tokens(byte_len),
            is_error: None,
        }
    }
}

/// Approximate token count for `byte_len` bytes of user-turn content: the
/// bytes/4 estimate, rounded up, with zero bytes as zero tokens.
pub fn bytes_to_approx_tokens(byte_len: u64) -> u64 {
    byte_len.div_ceil(4)
}

/// The non-empty entries of `parts` joined with `sep`, so an absent half
/// (a user turn with no assistant text) leaves no dangling separator.
pub(crate) fn join_nonempty(parts: &[&str], sep: &str) -> String {
    parts
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_counts_bytes_div_4_ceil() {
        let block = UserTurnBlock::text("hello");
        assert_eq!(block.byte_len, 5);
        assert_eq!(block.approx_tokens, 2);
        assert_eq!(block.kind, UserTurnBlockKind::Text);
    }

    #[test]
    fn bytes_to_approx_tokens_rounds_up() {
        assert_eq!(bytes_to_approx_tokens(0), 0);
        assert_eq!(bytes_to_approx_tokens(1), 1);
        assert_eq!(bytes_to_approx_tokens(4), 1);
        assert_eq!(bytes_to_approx_tokens(5), 2);
    }

    #[test]
    fn join_nonempty_skips_empty_parts() {
        assert_eq!(join_nonempty(&["a", "", "b"], "\n"), "a\nb");
        assert_eq!(join_nonempty(&["", ""], "\n"), "");
    }
}
