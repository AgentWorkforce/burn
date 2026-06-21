//! Shared helpers for the analyze module: money/number formatting, the
//! approximate token<->byte heuristic, and turn grouping.

use indexmap::IndexMap;

use crate::reader::TurnRecord;

/// Bucket turns by `session_id`, preserving first-seen (insertion) order so
/// the result iterates in the same order as the TS `Map<sessionId,
/// TurnRecord[]>` it ports — analyze fixtures depend on that ordering.
///
/// Turns within each session stay in input order; callers that need
/// turn-index order sort the returned `Vec`s themselves. Generic over the
/// turn iterator so both `&[TurnRecord]` and `&[&TurnRecord]` slices work
/// (`turns` directly, or `turns.iter().copied()` respectively).
pub(crate) fn group_turns_by_session<'a, I>(turns: I) -> IndexMap<String, Vec<&'a TurnRecord>>
where
    I: IntoIterator<Item = &'a TurnRecord>,
{
    let mut by_session: IndexMap<String, Vec<&'a TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session.entry(t.session_id.clone()).or_default().push(t);
    }
    by_session
}

/// Format a USD amount to 4 decimal places (`$0.1234`), matching the TS
/// finding adapters' money formatting.
pub(crate) fn fmt_usd(n: f64) -> String {
    format!("${n:.4}")
}

/// Approximate chars-per-token divisor. Anthropic's BPE averages ~3.5–4
/// chars/token for English; we use 4 to slightly under-estimate (better to
/// under-attribute cost than over-attribute). Shared by every approximate
/// token <-> byte conversion in the analyze module.
///
/// Note: this is a *ceiling* `÷4` heuristic. `context_delta` deliberately
/// uses floor division for its own `approx_tokens` field and is intentionally
/// not routed through these helpers.
const APPROX_BYTES_PER_TOKEN: u64 = 4;

/// Approximate token count from a UTF-8 byte length, rounded up.
/// `0` bytes → `0` tokens.
pub(crate) fn tokens_from_bytes(byte_len: u64) -> u64 {
    byte_len.div_ceil(APPROX_BYTES_PER_TOKEN)
}

/// Approximate token count from text measured in UTF-16 code units (matching
/// TS `string.length`), rounded up. Differs from [`tokens_from_bytes`] only on
/// non-ASCII input; used on the hotspots attribution path where fixtures must
/// stay bit-for-bit equivalent with the TS port (surrogate-pair behavior on
/// emoji included).
pub(crate) fn tokens_from_utf16_len(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(APPROX_BYTES_PER_TOKEN)
}

/// Inverse of [`tokens_from_bytes`]: approximate UTF-8 byte budget for a token
/// count. Used where a token threshold needs a character-unit ceiling.
pub(crate) fn bytes_from_tokens(tokens: u64) -> u64 {
    tokens * APPROX_BYTES_PER_TOKEN
}

/// Format an integer with thousands separators, matching JS
/// `Number.prototype.toLocaleString()` output for the en-US locale.
pub(crate) fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}
