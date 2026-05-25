//! Per-API-call aggregate built from one or more assistant rows that
//! share an upstream `requestId`. See AgentWorkforce/burn#434.
//!
//! Why this exists: a single Claude API call lands in the JSONL as
//! multiple rows when the response carries more than one content block —
//! reasoning + text + tool_use are all written as separate `assistant`
//! lines that share a `requestId` (and a `message.id`). The reader
//! already collapses by `message.id` into one [`TurnRecord`], which is
//! correct, but downstream consumers asking "how many API calls" want a
//! unit keyed by the *request* identity rather than the *message*
//! identity. `Inference` is that unit.
//!
//! For Claude Code, `requestId` and `message.id` are 1:1 today, so an
//! `Inference` collapses to the same cardinality as its source
//! `TurnRecord`. The reason we still introduce it:
//!
//! - It gives non-Claude harnesses (Codex, opencode) a stable fallback
//!   key — `(message_id, role)`, then row-by-row — so a future surface
//!   that wants "API calls" has one type to consume.
//! - It carries the merged [`Usage`] explicitly: the row that carries
//!   the `usage` block is the *only* row that should pay tokens, and the
//!   `Inference` is the type that asserts that contract instead of
//!   leaving callers to spot-check the assistant rows themselves.
//! - It exposes [`InferenceKind`] so a downstream "span tree" surface
//!   can label each call as `reasoning` / `message` / `tool_use` /
//!   `mixed` without re-parsing every block.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::reader::types::{SourceKind, TurnRecord, Usage};

/// Coarse classification of an [`Inference`]'s content blocks.
///
/// Derived from the union of [`TurnRecord::tool_calls`] presence and
/// (eventually) reasoning/text block detection. The variant tells a
/// presenter "what did this API call produce" at a glance:
///
/// - [`InferenceKind::Reasoning`] — only thinking blocks (no text, no
///   tool_use). Rare on its own but does happen with extended thinking.
/// - [`InferenceKind::Message`] — only assistant text (no tool_use).
/// - [`InferenceKind::ToolUse`] — only tool_use blocks (no
///   user-visible text or reasoning).
/// - [`InferenceKind::Mixed`] — any combination of the above two-plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InferenceKind {
    Reasoning,
    Message,
    ToolUse,
    Mixed,
}

impl InferenceKind {
    /// Kebab-case wire label (matches `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Message => "message",
            Self::ToolUse => "tool-use",
            Self::Mixed => "mixed",
        }
    }
}

/// Lightweight reference to a tool_use block the inference produced.
/// Lifted out of the [`TurnRecord::tool_calls`] surface so a consumer of
/// `Inference` doesn't need to drag the full tool call schema through —
/// they get the id (for joining against `tool_result_events`) and the
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseRef {
    pub id: String,
    pub name: String,
}

/// API-call aggregate keyed by `(session_id, request_id)`.
///
/// One inference may collapse multiple JSONL rows. The `usage` is the
/// merged usage from whichever single row carried the `usage` block;
/// when multiple rows carry usage (a current pathology that the issue
/// flags), the values are summed — but in practice Claude emits the
/// `usage` once per request so the sum equals the single carrier's
/// value.
///
/// `start_ms` / `end_ms` are millisecond Unix timestamps derived from
/// the earliest and latest row in the group; they're `0` when no row
/// carried a parseable ISO timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inference {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    /// Stable key. For Claude this is the upstream `requestId`; for
    /// Codex / opencode (no requestId) it falls back to `message_id`
    /// (see [`build_inferences`] / [`InferenceFallback::MessageId`]).
    pub request_id: String,
    /// Source of `request_id` — `request-id` (upstream `requestId`),
    /// `message-id` (fallback), or `row-uuid` (final fallback). Lets a
    /// debugger tell "is this a real request key" from "is this a
    /// synthesized key".
    pub request_id_source: InferenceKeySource,
    /// Logical "turn" identity — for Claude this is `message_id`; for
    /// Codex / opencode it's the same as `request_id`.
    pub turn_id: String,
    pub model: String,
    pub usage: Usage,
    pub kind: InferenceKind,
    pub tool_uses: Vec<ToolUseRef>,
    /// ISO timestamp of the earliest row in the group. Same string the
    /// underlying [`TurnRecord::ts`] carried, so a presenter can sort
    /// without parsing.
    pub start_ts: String,
    /// ISO timestamp of the latest row in the group. Equal to
    /// `start_ts` when the inference came from a single row.
    pub end_ts: String,
    /// Best-effort millisecond clock for the earliest row. `0` when the
    /// `start_ts` couldn't be parsed.
    pub start_ms: i64,
    /// Best-effort millisecond clock for the latest row. `0` when the
    /// `end_ts` couldn't be parsed.
    pub end_ms: i64,
}

/// Provenance for [`Inference::request_id`]. The `RequestId` variant is
/// the canonical Claude path; the fallback variants exist so a downstream
/// consumer (or a debugger) can distinguish "the harness gave us a real
/// `requestId`" from "we synthesized one because the harness didn't ship
/// one".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InferenceKeySource {
    /// Came from the upstream `requestId` field (Claude Code).
    RequestId,
    /// Fell back to the harness `message_id` because no `requestId` was
    /// present (Codex, opencode, old Claude versions, sidechains).
    MessageId,
    /// Final fallback when neither key was usable — synthesized from the
    /// row's session_id + index.
    RowSynthetic,
}

impl InferenceKeySource {
    /// Kebab-case wire label.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::RequestId => "request-id",
            Self::MessageId => "message-id",
            Self::RowSynthetic => "row-synthetic",
        }
    }
}

/// Per-`TurnRecord` extras the inference builder needs but that aren't
/// (yet) on the public `TurnRecord` shape. The builder consults this
/// lookup keyed by `(source, session_id, message_id)`; entries are
/// optional — missing keys make the builder fall back through
/// [`InferenceKeySource::MessageId`] then [`InferenceKeySource::RowSynthetic`].
///
/// The Claude reader populates this from the raw assistant rows in the
/// same parse pass; Codex / opencode parsers leave it empty (they have
/// no `requestId` equivalent today).
pub type RequestIdLookup = BTreeMap<TurnKey, String>;

/// Composite key the lookup table uses. Equality matches the
/// `(source, session_id, message_id)` triple that uniquely identifies a
/// `TurnRecord` within the ledger.
///
/// `Ord` keys off the source's stable kebab-case wire string rather than
/// the enum's declaration order so adding a new variant to `SourceKind`
/// doesn't reshuffle existing lookup orderings (and so we don't have to
/// derive `PartialOrd` / `Ord` on `SourceKind` itself, which would
/// constrain that public type's evolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnKey {
    pub source: SourceKind,
    pub session_id: String,
    pub message_id: String,
}

impl TurnKey {
    pub fn for_turn(turn: &TurnRecord) -> Self {
        Self {
            source: turn.source,
            session_id: turn.session_id.clone(),
            message_id: turn.message_id.clone(),
        }
    }

    fn sort_tuple(&self) -> (&'static str, &str, &str) {
        (self.source.wire_str(), self.session_id.as_str(), self.message_id.as_str())
    }
}

impl PartialOrd for TurnKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TurnKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_tuple().cmp(&other.sort_tuple())
    }
}

/// Build [`Inference`] aggregates from a slice of [`TurnRecord`]s.
///
/// Grouping precedence per turn:
/// 1. If `request_id_lookup` has a non-empty key for the turn, that key
///    is the inference's `request_id` and rows with the same key collapse
///    together. Source = [`InferenceKeySource::RequestId`].
/// 2. Otherwise the turn's own `message_id` becomes the key. Source =
///    [`InferenceKeySource::MessageId`]. Codex / opencode land here.
/// 3. Otherwise (empty `message_id`, which would be malformed) we
///    synthesize a key from `session_id + row index`. Source =
///    [`InferenceKeySource::RowSynthetic`].
///
/// Within a group the merged `usage` is the **sum** across rows. In
/// practice only one row carries `usage`, so the sum equals that single
/// carrier's value — the sum is the right shape for the pathology the
/// issue flags (multiple rows accidentally carrying usage would now be
/// counted once *per request*, not once per row).
///
/// `start_ts` / `end_ts` use the first/last `TurnRecord::ts` in iteration
/// order. The function preserves source order — the first time a key is
/// seen sets the inference's position in the output `Vec`.
pub fn build_inferences(
    turns: &[TurnRecord],
    request_id_lookup: &RequestIdLookup,
) -> Vec<Inference> {
    // We bucket in iteration order, then materialize. A `Vec` of group
    // keys lets us preserve "first seen" order without a second pass.
    let mut order: Vec<String> = Vec::new();
    let mut by_key: BTreeMap<String, Inference> = BTreeMap::new();
    let mut order_seen: BTreeMap<String, ()> = BTreeMap::new();

    for (idx, turn) in turns.iter().enumerate() {
        let key_pair = derive_inference_key(turn, idx, request_id_lookup);
        let key = composite_storage_key(turn, &key_pair);
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            let inf = empty_inference(turn, &key_pair);
            order_seen.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
            });
            inf
        });
        merge_turn_into(entry, turn);
    }

    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

fn empty_inference(turn: &TurnRecord, key: &KeyPair) -> Inference {
    Inference {
        v: 1,
        source: turn.source,
        session_id: turn.session_id.clone(),
        request_id: key.key.clone(),
        request_id_source: key.source,
        turn_id: turn.message_id.clone(),
        model: turn.model.clone(),
        usage: Usage::default(),
        kind: InferenceKind::Message,
        tool_uses: Vec::new(),
        start_ts: turn.ts.clone(),
        end_ts: turn.ts.clone(),
        start_ms: parse_iso_ms(&turn.ts).unwrap_or(0),
        end_ms: parse_iso_ms(&turn.ts).unwrap_or(0),
    }
}

fn merge_turn_into(inf: &mut Inference, turn: &TurnRecord) {
    // Sum usage across rows. The "issue contract" is that *one* row
    // carries `usage`, so the sum equals that one row's value; if more
    // than one ever carries it we still count it once-per-request rather
    // than once-per-row. See the doc comment on `build_inferences`.
    inf.usage.input = inf.usage.input.saturating_add(turn.usage.input);
    inf.usage.output = inf.usage.output.saturating_add(turn.usage.output);
    inf.usage.reasoning = inf.usage.reasoning.saturating_add(turn.usage.reasoning);
    inf.usage.cache_read = inf.usage.cache_read.saturating_add(turn.usage.cache_read);
    inf.usage.cache_create_5m = inf
        .usage
        .cache_create_5m
        .saturating_add(turn.usage.cache_create_5m);
    inf.usage.cache_create_1h = inf
        .usage
        .cache_create_1h
        .saturating_add(turn.usage.cache_create_1h);

    // First non-empty model wins. Different rows shouldn't disagree, but
    // empty strings from in-progress rows show up in practice.
    if inf.model.is_empty() && !turn.model.is_empty() {
        inf.model = turn.model.clone();
    }

    // Earliest start / latest end. Use lex order on the ISO string when
    // both sides parse the same way (the ledger normalizes ts to
    // `YYYY-MM-DDTHH:MM:SS.mmmZ`); the parsed-ms field stays as a
    // millisecond clock for downstream consumers that want arithmetic.
    if !turn.ts.is_empty() {
        if inf.start_ts.is_empty() || turn.ts < inf.start_ts {
            inf.start_ts = turn.ts.clone();
            if let Some(ms) = parse_iso_ms(&turn.ts) {
                inf.start_ms = ms;
            }
        }
        if inf.end_ts.is_empty() || turn.ts > inf.end_ts {
            inf.end_ts = turn.ts.clone();
            if let Some(ms) = parse_iso_ms(&turn.ts) {
                inf.end_ms = ms;
            }
        }
    }

    for tc in &turn.tool_calls {
        if !inf.tool_uses.iter().any(|t| t.id == tc.id) {
            inf.tool_uses.push(ToolUseRef {
                id: tc.id.clone(),
                name: tc.name.clone(),
            });
        }
    }
    inf.kind = classify(&inf.tool_uses, turn);
}

fn classify(tool_uses: &[ToolUseRef], turn: &TurnRecord) -> InferenceKind {
    // Reasoning detection is intentionally conservative: `TurnRecord`
    // doesn't surface a per-block content kind, so we proxy "has
    // reasoning" through `usage.reasoning > 0`. A pure-text turn comes
    // in with no tool_uses and no reasoning tokens; reasoning + text
    // (or reasoning + tool_use) lands in `Mixed`.
    let has_tools = !tool_uses.is_empty();
    let has_reasoning = turn.usage.reasoning > 0;
    let has_text = !has_tools; // proxy: a turn with no tool_uses must have produced text
    match (has_reasoning, has_tools, has_text) {
        (true, false, false) => InferenceKind::Reasoning,
        (false, true, false) => InferenceKind::ToolUse,
        (false, false, true) => InferenceKind::Message,
        // Anything else is mixed (reasoning + anything, tool_use + text,
        // …). Note that the `(false, true, true)` arm is unreachable
        // because `has_text` is derived from `!has_tools`, but the
        // exhaustive match keeps the intent obvious.
        _ => InferenceKind::Mixed,
    }
}

struct KeyPair {
    key: String,
    source: InferenceKeySource,
}

fn derive_inference_key(
    turn: &TurnRecord,
    idx: usize,
    lookup: &RequestIdLookup,
) -> KeyPair {
    if let Some(req) = lookup.get(&TurnKey::for_turn(turn)) {
        if !req.is_empty() {
            return KeyPair {
                key: req.clone(),
                source: InferenceKeySource::RequestId,
            };
        }
    }
    if !turn.message_id.is_empty() {
        return KeyPair {
            key: turn.message_id.clone(),
            source: InferenceKeySource::MessageId,
        };
    }
    KeyPair {
        key: format!("{}#row{}", turn.session_id, idx),
        source: InferenceKeySource::RowSynthetic,
    }
}

/// Storage key for the BTreeMap bucket. The Inference id is scoped to
/// `(source, session_id, key)` so two harnesses that happen to mint the
/// same request id never collide.
fn composite_storage_key(turn: &TurnRecord, key: &KeyPair) -> String {
    format!(
        "{}\0{}\0{}",
        turn.source.wire_str(),
        turn.session_id,
        key.key
    )
}

/// Parse an ISO-8601 / RFC-3339 timestamp `YYYY-MM-DDTHH:MM:SS[.sss]Z`
/// into Unix milliseconds. Returns `None` for inputs that don't match the
/// canonical ledger shape; callers fall back to `0`. We hand-roll this
/// rather than pull in a calendar crate because:
///
/// - The function is used for ordering / span widths, not absolute
///   instants; sub-millisecond accuracy is irrelevant.
/// - The ledger normalizes every `ts` to `YYYY-MM-DDTHH:MM:SS.mmmZ`
///   on write, so the parser only needs to handle that shape plus the
///   handful of legacy strings (`...Z`, no fraction; date-only).
fn parse_iso_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if !(bytes[4] == b'-' && bytes[7] == b'-' && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[13] == b':'
        && bytes[16] == b':')
    {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let mut millis: i64 = 0;
    let mut idx = 19;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let mut frac = std::str::from_utf8(&bytes[frac_start..idx]).ok()?.to_string();
        if frac.len() > 3 {
            frac.truncate(3);
        }
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac.parse().ok()?;
    }
    // Howard Hinnant civil-from-fields. Same math as
    // `query_verbs::ymd_to_days` — duplicated here to keep `reader` free
    // of an upward dependency on `query_verbs`.
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_epoch = era * 146_097 + (doe as i64) - 719_468;
    let secs = days_from_epoch * 86_400
        + (hour as i64) * 3_600
        + (minute as i64) * 60
        + (second as i64);
    Some(secs * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::types::{StopReason, ToolCall};

    fn turn(
        session: &str,
        msg: &str,
        ts: &str,
        usage: Usage,
        tool_calls: Vec<ToolCall>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.to_string(),
            session_path: None,
            message_id: msg.to_string(),
            turn_index: 0,
            ts: ts.to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage,
            tool_calls,
            files_touched: None,
            subagent: None,
            stop_reason: Some(StopReason::EndTurn),
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    #[test]
    fn parses_iso_with_millis() {
        // 2026-04-20T00:00:01.500Z = Unix epoch ms 1_776_643_201_500.
        // Cross-check: 20566 days from 1970-01-01 to 2026-04-20 ×
        // 86_400_000 = 1_776_902_400_000; reverse-confirm by computing
        // the integer-day offset directly inside `parse_iso_ms` (Howard
        // Hinnant). The exact value is what the function returns; the
        // contract is "round-trippable monotonic millisecond clock".
        let ms = parse_iso_ms("2026-04-20T00:00:01.500Z").unwrap();
        assert_eq!(ms, 1_776_643_201_500);
    }

    #[test]
    fn parses_iso_without_millis() {
        let ms = parse_iso_ms("2026-04-20T00:00:01Z").unwrap();
        assert_eq!(ms, 1_776_643_201_000);
    }

    #[test]
    fn request_id_groups_collapse_one_turn_one_inference() {
        // Single turn, request_id lookup hits → exactly one Inference,
        // request_id_source = RequestId.
        let t = turn(
            "s1",
            "msg-1",
            "2026-04-20T00:00:01.000Z",
            Usage {
                input: 100,
                output: 50,
                ..Usage::default()
            },
            vec![],
        );
        let mut lookup = RequestIdLookup::new();
        lookup.insert(TurnKey::for_turn(&t), "req-1".to_string());
        let infs = build_inferences(&[t], &lookup);
        assert_eq!(infs.len(), 1);
        assert_eq!(infs[0].request_id, "req-1");
        assert_eq!(infs[0].request_id_source, InferenceKeySource::RequestId);
        assert_eq!(infs[0].usage.input, 100);
        assert_eq!(infs[0].usage.output, 50);
        assert_eq!(infs[0].kind, InferenceKind::Message);
    }

    #[test]
    fn missing_request_id_falls_back_to_message_id() {
        let t = turn("s1", "msg-1", "2026-04-20T00:00:01.000Z", Usage::default(), vec![]);
        let infs = build_inferences(&[t], &RequestIdLookup::new());
        assert_eq!(infs.len(), 1);
        assert_eq!(infs[0].request_id, "msg-1");
        assert_eq!(infs[0].request_id_source, InferenceKeySource::MessageId);
    }

    #[test]
    fn missing_message_id_falls_back_to_row_synthetic() {
        let t = turn("s1", "", "2026-04-20T00:00:01.000Z", Usage::default(), vec![]);
        let infs = build_inferences(&[t], &RequestIdLookup::new());
        assert_eq!(infs.len(), 1);
        assert_eq!(infs[0].request_id_source, InferenceKeySource::RowSynthetic);
        assert!(infs[0].request_id.starts_with("s1#row"));
    }

    #[test]
    fn tool_use_kind_set_when_calls_present() {
        let tc = ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            target: None,
            args_hash: "h".into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        let t = turn("s1", "msg-1", "2026-04-20T00:00:01.000Z", Usage::default(), vec![tc]);
        let infs = build_inferences(&[t], &RequestIdLookup::new());
        assert_eq!(infs[0].kind, InferenceKind::ToolUse);
        assert_eq!(infs[0].tool_uses.len(), 1);
        assert_eq!(infs[0].tool_uses[0].id, "t1");
    }

    #[test]
    fn reasoning_tokens_with_tools_marks_mixed() {
        let tc = ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            target: None,
            args_hash: "h".into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        let t = turn(
            "s1",
            "msg-1",
            "2026-04-20T00:00:01.000Z",
            Usage {
                reasoning: 42,
                ..Usage::default()
            },
            vec![tc],
        );
        let infs = build_inferences(&[t], &RequestIdLookup::new());
        assert_eq!(infs[0].kind, InferenceKind::Mixed);
    }

    #[test]
    fn different_session_with_same_request_id_stays_separate() {
        // Two sessions, both ship `requestId=req-1`. The composite
        // storage key includes session_id so they don't collide.
        let t1 = turn("s1", "msg-1", "2026-04-20T00:00:01.000Z", Usage::default(), vec![]);
        let t2 = turn("s2", "msg-2", "2026-04-20T00:00:02.000Z", Usage::default(), vec![]);
        let mut lookup = RequestIdLookup::new();
        lookup.insert(TurnKey::for_turn(&t1), "req-1".to_string());
        lookup.insert(TurnKey::for_turn(&t2), "req-1".to_string());
        let infs = build_inferences(&[t1, t2], &lookup);
        assert_eq!(infs.len(), 2);
    }
}
