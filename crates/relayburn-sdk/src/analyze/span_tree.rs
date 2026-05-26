//! Per-turn span tree — derived analytical primitive over a [`TurnRecord`]
//! and its surrounding data (tool_result_events, content sidecars, subagent
//! transcripts). See AgentWorkforce/burn#430.
//!
//! # Why this module exists
//!
//! Burn's analytical units today are flat rows: [`TurnRecord`] /
//! [`ToolResultEventRecord`] / [`ContentRecord`]. That shape is fine for
//! tabular aggregation (summary, hotspots, overhead) but loses the
//! hierarchical structure of a turn — inferences within a turn, tool_uses
//! nested inside an inference, subagent fanouts nested inside a tool_use.
//! Anything that wants to answer "where in this turn did the cost / time /
//! context blow up?" has to re-derive that hierarchy ad-hoc per call site.
//!
//! The span tree is the canonical hierarchy. Scalars (token totals, tool
//! counts) project off the tree rather than the other way around. The
//! tree is derived per call — there is no `span_trees` SQLite table; the
//! issue's "default position: always derive" rule applies.
//!
//! # Hierarchy
//!
//! ```text
//! Turn (root)
//! ├── UserPrompt
//! ├── Inference                    <- one per requestId (Claude #434)
//! │   ├── ToolUse (Bash)
//! │   │   └── ToolResult
//! │   ├── ToolUse (Task)           <- subagent dispatch
//! │   │   ├── ToolResult
//! │   │   └── Subagent             <- nested span tree from agent-*.jsonl
//! │   │       └── ...
//! │   └── ToolUse (Skill)
//! └── Inference                    <- if turn produced multiple API calls
//!     └── ...
//! ```
//!
//! # Attribute keys
//!
//! Downstream consumers (inference-flow DAG #431, context-delta #432,
//! hotspots, MCP presenters) rely on these attribute keys being stable.
//! Every key documented here is what the builders emit; consumers should
//! treat any key NOT in this list as advisory and not present-by-contract.
//!
//! Token usage (all `u64`, encoded as [`AttrValue::Int`]):
//! - `tokens.input`         — prompt input tokens for this span.
//! - `tokens.output`        — completion output tokens for this span.
//! - `tokens.cache_read`    — cached-prefix tokens read for this span.
//! - `tokens.cache_write`   — sum of `cache_create_5m` + `cache_create_1h`
//!   for this span; the 5m/1h split is harness-specific and not exposed here.
//! - `tokens.reasoning`     — extended-thinking reasoning tokens.
//!
//! Identity / context (encoded as [`AttrValue::String`]):
//! - `model`                — model identifier the inference ran against.
//! - `request_id`           — upstream Claude `requestId` (or the fallback
//!   key — see [`crate::reader::InferenceKeySource`]).
//! - `agent_id`             — `<agentId>` filename portion of a subagent
//!   sidecar transcript.
//! - `tool_use_id`          — id of a `tool_use` block; matches the
//!   paired `tool_result` event's `tool_use_id`.
//! - `cwd`                  — working directory active for the turn (when
//!   recorded by the harness; absent today on the `TurnRecord` shape but
//!   reserved here so future captures need no new key).
//! - `mode`                 — harness mode (plan / accept-edits / etc.) —
//!   reserved for the same reason as `cwd`.
//! - `stop_reason`          — kebab-case [`StopReason`] wire string on the
//!   root span when the trailing assistant row carried one.
//!
//! Flags (encoded as [`AttrValue::Bool`]):
//! - `unattached`           — `true` on `Subagent` spans whose sidecar
//!   could not be paired to a parent `ToolUse`.
//!
//! # Status mapping
//!
//! OTel's status model is `Ok | Error { msg }` (we intentionally drop
//! `Unset` — every span here is a completed historical record). We map
//! burn's error signals as follows:
//!
//! | Signal                                  | Status                                    |
//! | --------------------------------------- | ----------------------------------------- |
//! | `tool_use.is_error == true`             | `Error { msg: "tool_error" }` on the      |
//! |                                         | `ToolUse` span; bubbles to the parent     |
//! |                                         | `Inference` and `Turn` root.              |
//! | `stop_reason == Refusal`                | `Error { msg: "refusal" }` on the root.   |
//! | `stop_reason == MaxTokens`              | `Error { msg: "max_tokens" }` on the root.|
//! | Any other / absent                      | `Ok`.                                     |
//!
//! Error propagation is bottom-up: a single erroring `ToolUse` taints the
//! enclosing `Inference` and the `Turn` root with `Error { msg: "child_error" }`
//! so a tree-level "is this turn fine?" check is one field access on the
//! root.
//!
//! # Time source
//!
//! Spans carry `start_ms` / `end_ms` as Unix milliseconds. The Claude
//! builder pulls these from each [`TurnRecord::ts`] and
//! [`crate::reader::Inference::start_ms`] / `end_ms` (already a millisecond
//! clock). Tool_use spans inherit their inference's range — Claude does
//! not record per-tool-use wall-clock timestamps on the assistant row.
//! `ToolResult` spans use the paired event's `ts` when available, falling
//! back to the parent `ToolUse`'s end. Missing timestamps stay at `0` —
//! callers building Gantt-style layouts should use row order
//! (`turn_index`, source iteration order) as the fallback positional key.
//!
//! # Wire format
//!
//! Every type in this module serializes with [`serde`] in kebab-case to
//! match the repo convention (see [`crate::reader::StopReason`] and
//! [`ActivityCategory`] for the established pattern). The shape mirrors
//! agent-profiler's `SpanNode` so consumers from that ecosystem can
//! adopt this surface with minimal translation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What kind of node a [`SpanNode`] represents in the per-turn hierarchy.
///
/// Variants are exhaustive — adding a new kind is a deliberate schema
/// change that downstream consumers (inference-flow DAG, context-delta
/// attribution, MCP presenters) need to handle. We intentionally do NOT
/// mark this `#[non_exhaustive]`: the type is a stable analytical
/// primitive and breaking enum changes deserve to be source-level visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpanKind {
    /// The root node of a per-turn tree. Holds the user's prompt + every
    /// API call the harness made answering it.
    Turn,
    /// One Claude / Codex / etc. API call. Children are the tool_uses
    /// the model emitted (if any) and any reasoning / message metadata.
    Inference,
    /// A single `tool_use` block emitted by the model. Children are the
    /// paired `ToolResult` (if any) and any nested `Subagent` subtree.
    ToolUse,
    /// A subagent dispatched by a `Task` tool_use. The subtree under this
    /// node mirrors the parent shape — `Turn -> Inference -> ToolUse ...`
    /// — but rooted in the sidecar transcript. Sidecars whose
    /// `agent_id` could not be paired to a parent `ToolUse` become
    /// orphan `Subagent` spans under the `Turn` root with
    /// `attributes["unattached"] = true`.
    Subagent,
    /// A skill (slash-command-style) invocation synthesized from a triad
    /// of harness events. Reserved variant — current builders do not emit
    /// this kind; future skill plumbing fills it in.
    Skill,
    /// Plain user prompt text — first child of every `Turn` root.
    UserPrompt,
    /// Tool result envelope paired to a `ToolUse` by `tool_use_id`. Carries
    /// the result's byte size and error flag on its attributes.
    ToolResult,
}

impl SpanKind {
    /// Kebab-case wire label.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Inference => "inference",
            Self::ToolUse => "tool-use",
            Self::Subagent => "subagent",
            Self::Skill => "skill",
            Self::UserPrompt => "user-prompt",
            Self::ToolResult => "tool-result",
        }
    }
}

/// OTel-aligned status code for a [`SpanNode`]. We drop OTel's `Unset`
/// state — every span the builders emit is a completed historical
/// record, so the meaningful split is `Ok` vs `Error`.
///
/// The `Error` variant carries a stable message string from a tight
/// vocabulary documented at the module level:
///
/// - `"tool_error"` — child `ToolUse` failed (`is_error == true`).
/// - `"refusal"` — root assistant turn ended with `stop_reason = refusal`.
/// - `"max_tokens"` — root assistant turn ended with `stop_reason = max_tokens`.
/// - `"child_error"` — propagated up from an erroring descendant.
///
/// Downstream consumers should treat any other string as opaque
/// (display-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "kebab-case")]
pub enum SpanStatus {
    /// The span completed without a known error signal.
    Ok,
    /// The span (or a descendant) reported an error. The `msg` is
    /// drawn from the vocabulary documented above.
    Error {
        /// Short, stable error label. See variant doc for the vocabulary.
        msg: String,
    },
}

impl SpanStatus {
    /// `true` iff this status carries an error label.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

/// Typed scalar attribute value. Mirrors the agent-profiler attribute
/// shape (`string | number | bool`), split here into Rust-native types so
/// numeric attributes don't lose precision through a `serde_json::Value`
/// round-trip.
///
/// `Int` covers the token counters (always non-negative, but signed to
/// leave room for derived deltas without a separate variant). `Float`
/// covers cost-like accumulators a future analyzer might attach. `String`
/// covers model / request_id / agent_id / etc. `Bool` is for flags such
/// as `unattached`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    /// Free-text identity or label.
    String(String),
    /// Signed integer. Token counts use this.
    Int(i64),
    /// Floating-point scalar. Reserved for cost / latency attributes.
    Float(f64),
    /// Boolean flag.
    Bool(bool),
}

impl AttrValue {
    /// Convenience constructor: `AttrValue::str("...")`.
    pub fn str(s: impl Into<String>) -> Self {
        Self::String(s.into())
    }
}

// No `Eq` impl: `AttrValue::Float(f64)` makes `Eq`'s reflexivity
// contract impossible — `NaN != NaN` violates `a == a`. The span tree
// stores attributes in a `BTreeMap<String, AttrValue>`, which only
// requires `Ord` on its keys (always `String`), so `PartialEq` is
// sufficient. Consumers that want NaN equality should normalize
// beforehand.

/// One point-in-time event attached to a span. Modeled on OTel's
/// `SpanEvent` so a future exporter can hand-off cleanly.
///
/// Builders use events for things that have a single instant but no
/// duration — for example a compaction tick on the `Turn` root, or a
/// retry marker on an `Inference`. Current builders emit no events; the
/// type is part of the locked-in shape so downstream consumers can rely
/// on it being there from day one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanEvent {
    /// Wall-clock millisecond timestamp for the event.
    pub ts: i64,
    /// Stable event name. Vocabulary will be documented once builders
    /// start emitting events; treat unknown names as opaque.
    pub name: String,
    /// Event-scoped attributes. Same `AttrValue` shape as the span's
    /// own attributes so consumers can flatten them uniformly.
    pub attributes: BTreeMap<String, AttrValue>,
}

/// One node in a [`TurnSpanTree`].
///
/// `attributes` and `events` order is alphabetical via [`BTreeMap`] so
/// serialization is byte-stable across runs — required by the
/// presentation-test golden fixtures and useful for any downstream that
/// hashes the tree.
///
/// `children` order is meaningful: builders emit children in causal
/// (parent → child) order matching the source row order, so a depth-
/// first traversal reproduces the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanNode {
    /// What kind of node this is. See [`SpanKind`].
    pub kind: SpanKind,
    /// Human-readable label for the span — for `ToolUse` this is the
    /// tool name, for `Inference` it's the model, for `Turn` it's a
    /// stable `"turn"` literal. Consumers should treat this as
    /// display-only and key off `kind` for routing.
    pub name: String,
    /// Span start (Unix milliseconds). `0` when no timestamp was
    /// available.
    pub start_ms: i64,
    /// Span end (Unix milliseconds). `0` when no timestamp was
    /// available. Equal to `start_ms` for instant-like spans
    /// (`UserPrompt`, `ToolResult`).
    pub end_ms: i64,
    /// OTel-aligned status. See [`SpanStatus`].
    pub status: SpanStatus,
    /// Attribute bag. See the module-level "Attribute keys" table for
    /// the locked-in vocabulary.
    pub attributes: BTreeMap<String, AttrValue>,
    /// Point-in-time events attached to this span. See [`SpanEvent`].
    pub events: Vec<SpanEvent>,
    /// Causally-ordered children. See type-level docs for ordering
    /// rules.
    pub children: Vec<SpanNode>,
}

impl SpanNode {
    /// Build a leaf span (no children, no events) of the given kind.
    /// Used by the harness builders as the spine of node construction;
    /// they then push children and attributes via the public fields.
    pub fn new(kind: SpanKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            start_ms: 0,
            end_ms: 0,
            status: SpanStatus::Ok,
            attributes: BTreeMap::new(),
            events: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Set the time range on the span — convenience for builders so
    /// they don't need to touch the two fields independently.
    pub fn with_range(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.start_ms = start_ms;
        self.end_ms = end_ms;
        self
    }

    /// Insert an attribute. Returns `&mut Self` so builders can chain
    /// multiple insertions during construction.
    pub fn set_attr(&mut self, key: impl Into<String>, value: AttrValue) -> &mut Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Mark this span as errored with the given vocabulary label.
    pub fn set_error(&mut self, msg: impl Into<String>) -> &mut Self {
        self.status = SpanStatus::Error { msg: msg.into() };
        self
    }

    /// Depth-first iterator yielding `&SpanNode` for `self` and every
    /// descendant. Useful for "sum scalars across the tree" projections.
    pub fn iter_dfs(&self) -> SpanDfsIter<'_> {
        SpanDfsIter { stack: vec![self] }
    }
}

/// Depth-first iterator over a [`SpanNode`] and its descendants. Yielded
/// by [`SpanNode::iter_dfs`]. The traversal visits a node before its
/// children, matching the order the builders construct the tree in.
pub struct SpanDfsIter<'a> {
    stack: Vec<&'a SpanNode>,
}

impl<'a> Iterator for SpanDfsIter<'a> {
    type Item = &'a SpanNode;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        // Push children in reverse so the next pop yields the first
        // child — depth-first, left-to-right.
        for child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

/// Per-turn span tree. The root is always a [`SpanKind::Turn`] node;
/// every other span lives somewhere underneath.
///
/// `session_id` and `turn_id` together uniquely identify the source
/// turn in the ledger — they match [`crate::reader::TurnRecord::session_id`]
/// and `message_id` respectively. `turn_number` is `TurnRecord::turn_index`
/// promoted to `u32` for OTel compatibility (agent-profiler exports use
/// a 32-bit turn number).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSpanTree {
    /// Session this turn belongs to.
    pub session_id: String,
    /// Stable per-session turn identifier. For Claude this is the
    /// `message_id` carried on the trailing assistant row of the turn.
    pub turn_id: String,
    /// 0-indexed position of the turn within the session. Promoted from
    /// [`crate::reader::TurnRecord::turn_index`] (`u64`) to `u32` for
    /// OTel / agent-profiler wire compatibility.
    pub turn_number: u32,
    /// Root span — always [`SpanKind::Turn`].
    pub root: SpanNode,
}

impl TurnSpanTree {
    /// Sum a scalar token attribute across every span in the tree.
    /// Returns `0` when no span carries the key (or when the key is
    /// present but non-integer — silent fallback so callers can use
    /// this on attribute keys that are intermittently present).
    ///
    /// This is the projection helper consumers (hotspots, overhead,
    /// the agent-profiler-compatible scalar block) call. Per the
    /// issue's contract, the root's per-key sum should equal the
    /// underlying [`TurnRecord::usage`] field within rounding — see
    /// the unit tests for the assertion.
    pub fn sum_attr_int(&self, key: &str) -> i64 {
        self.root
            .iter_dfs()
            .filter_map(|n| match n.attributes.get(key) {
                Some(AttrValue::Int(i)) => Some(*i),
                _ => None,
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_kind_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&SpanKind::Turn).unwrap(), "\"turn\"");
        assert_eq!(
            serde_json::to_string(&SpanKind::ToolUse).unwrap(),
            "\"tool-use\""
        );
        assert_eq!(
            serde_json::to_string(&SpanKind::UserPrompt).unwrap(),
            "\"user-prompt\""
        );
        assert_eq!(
            serde_json::to_string(&SpanKind::ToolResult).unwrap(),
            "\"tool-result\""
        );
    }

    #[test]
    fn span_kind_round_trips() {
        for k in [
            SpanKind::Turn,
            SpanKind::Inference,
            SpanKind::ToolUse,
            SpanKind::Subagent,
            SpanKind::Skill,
            SpanKind::UserPrompt,
            SpanKind::ToolResult,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: SpanKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn span_status_serializes_tagged_kebab_case() {
        let ok = serde_json::to_string(&SpanStatus::Ok).unwrap();
        assert_eq!(ok, "{\"code\":\"ok\"}");
        let err = serde_json::to_string(&SpanStatus::Error {
            msg: "max_tokens".into(),
        })
        .unwrap();
        assert!(err.contains("\"code\":\"error\""), "got {err}");
        assert!(err.contains("\"msg\":\"max_tokens\""), "got {err}");
        let back: SpanStatus = serde_json::from_str(&err).unwrap();
        assert!(back.is_error());
    }

    #[test]
    fn attr_value_string_round_trips() {
        let v = AttrValue::String("claude-sonnet".into());
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, "\"claude-sonnet\"");
        let back: AttrValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn attr_value_int_round_trips_as_bare_number() {
        let v = AttrValue::Int(42);
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, "42");
        let back: AttrValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn attr_value_bool_round_trips() {
        let v = AttrValue::Bool(true);
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, "true");
        let back: AttrValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn span_node_camel_case_fields() {
        let n = SpanNode::new(SpanKind::ToolUse, "Bash").with_range(100, 200);
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains("\"startMs\":100"), "got {s}");
        assert!(s.contains("\"endMs\":200"), "got {s}");
        assert!(s.contains("\"kind\":\"tool-use\""), "got {s}");
        assert!(s.contains("\"name\":\"Bash\""), "got {s}");
        // Empty attrs / events should still serialize as `{}` / `[]`
        // so downstream consumers can rely on the field being present.
        assert!(s.contains("\"attributes\":{}"), "got {s}");
        assert!(s.contains("\"events\":[]"), "got {s}");
        assert!(s.contains("\"children\":[]"), "got {s}");
    }

    #[test]
    fn span_node_set_attr_and_error_chains() {
        let mut n = SpanNode::new(SpanKind::Inference, "claude-sonnet");
        n.set_attr("model", AttrValue::str("claude-sonnet"));
        n.set_attr("tokens.input", AttrValue::Int(100));
        n.set_error("max_tokens");
        assert_eq!(n.attributes.len(), 2);
        assert!(n.status.is_error());
        match &n.status {
            SpanStatus::Error { msg } => assert_eq!(msg, "max_tokens"),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn turn_span_tree_camel_case_fields() {
        let tree = TurnSpanTree {
            session_id: "sess-1".into(),
            turn_id: "msg-1".into(),
            turn_number: 0,
            root: SpanNode::new(SpanKind::Turn, "turn"),
        };
        let s = serde_json::to_string(&tree).unwrap();
        assert!(s.contains("\"sessionId\":\"sess-1\""), "got {s}");
        assert!(s.contains("\"turnId\":\"msg-1\""), "got {s}");
        assert!(s.contains("\"turnNumber\":0"), "got {s}");
        let back: TurnSpanTree = serde_json::from_str(&s).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn iter_dfs_visits_parent_before_children() {
        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        let mut a = SpanNode::new(SpanKind::Inference, "a");
        a.children.push(SpanNode::new(SpanKind::ToolUse, "Bash"));
        a.children.push(SpanNode::new(SpanKind::ToolUse, "Read"));
        let b = SpanNode::new(SpanKind::Inference, "b");
        root.children.push(a);
        root.children.push(b);
        let names: Vec<&str> = root.iter_dfs().map(|n| n.name.as_str()).collect();
        // Pre-order: root, a, Bash, Read, b.
        assert_eq!(names, vec!["turn", "a", "Bash", "Read", "b"]);
    }

    #[test]
    fn sum_attr_int_aggregates_across_tree() {
        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.set_attr("tokens.input", AttrValue::Int(100));
        let mut child = SpanNode::new(SpanKind::Inference, "inf");
        child.set_attr("tokens.input", AttrValue::Int(50));
        root.children.push(child);
        let tree = TurnSpanTree {
            session_id: "s".into(),
            turn_id: "m".into(),
            turn_number: 0,
            root,
        };
        // DFS picks up both, sum is 150.
        assert_eq!(tree.sum_attr_int("tokens.input"), 150);
        assert_eq!(tree.sum_attr_int("tokens.missing"), 0);
    }

    #[test]
    fn span_status_is_error_helper() {
        assert!(!SpanStatus::Ok.is_error());
        assert!(SpanStatus::Error { msg: "x".into() }.is_error());
    }
}
