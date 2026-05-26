//! Per-session inference-flow DAG — pure projection from a session's
//! [`TurnSpanTree`]s into a renderable graph. See AgentWorkforce/burn#431.
//!
//! # Why this module exists
//!
//! The span tree (issue #430) is the canonical per-turn hierarchy. The
//! flow graph is the per-**session** projection of those trees into a
//! 2-D DAG suitable for the inference-flow visualization:
//!
//! - **X-axis**: turn number — one column per turn.
//! - **Y-axis**: rail index — the main rail at `y=0`, every dispatched
//!   subagent on its own rail underneath, inheriting the dispatching
//!   inference's Y so the branch point is visually obvious.
//!
//! Renderers (SVG, Mermaid, future React-Flow) consume the laid-out
//! [`FlowGraph`] directly; layout is **not** their concern.
//!
//! # Node identity
//!
//! Node IDs are stable string keys derived from the source data:
//!
//! - `"{turn_id}:inf-{index}"` for inference nodes (one per `Inference`
//!   span under a turn root).
//! - `"{turn_id}:tu-{tool_use_id}"` for `ToolUse` / `Skill` nodes.
//! - `"{turn_id}:sa-{agent_id}"` for `Subagent` rail roots.
//!
//! The id is the same whether the node is reachable via the main rail
//! or via a nested subagent rail — consumers can therefore dedupe by
//! id without losing edges.
//!
//! # Edge semantics
//!
//! - [`FlowEdgeKind::Default`] — sequential within a rail.
//! - [`FlowEdgeKind::Dispatch`] — main rail's `Task` `ToolUse` →
//!   first node on the spawned subagent rail.
//! - [`FlowEdgeKind::Return`] — last node of a subagent rail → the
//!   next main-rail inference (if any).
//! - [`FlowEdgeKind::Subagent`] — sequential within a subagent rail.
//! - [`FlowEdgeKind::Unattached`] — connects the synthetic turn anchor
//!   to a `Subagent` flagged `attributes["unattached"] = true`. Surfaces
//!   the orphan case loudly so renderers can highlight it.

use serde::{Deserialize, Serialize};

use crate::analyze::span_tree::{AttrValue, SpanKind, SpanNode, SpanStatus, TurnSpanTree};

/// Horizontal spacing between turn columns, in pixels. Mirrors the
/// agent-profiler reference value so embedders that adopt this graph
/// can reuse the same renderer code paths.
pub const INTER_TURN_GAP: i32 = 96;

/// Vertical spacing between rails (main or subagent), in pixels.
pub const RAIL_GAP: i32 = 32;

/// Default `--max-turns` cap — see the CLI surface. Layouts wider than
/// ~50 columns get unreadable in static SVG / Mermaid; embedders that
/// want the full session can pass [`FlowOpts::max_turns`] explicitly.
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// What kind of node a [`FlowNode`] represents in the flow DAG. Mirrors
/// agent-profiler's node-kind registry so embedders that ship their own
/// React-Flow renderer can adopt this surface with minimal translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowNodeKind {
    /// One API call — projection of a [`SpanKind::Inference`] span.
    Inference,
    /// One `tool_use` block emitted by the model.
    ToolUse,
    /// A dispatched subagent's rail anchor — projection of a
    /// [`SpanKind::Subagent`] span.
    Subagent,
    /// A skill (slash-command-style) invocation — projection of a
    /// [`SpanKind::Skill`] span.
    Skill,
}

impl FlowNodeKind {
    /// Kebab-case wire label.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::ToolUse => "tool-use",
            Self::Subagent => "subagent",
            Self::Skill => "skill",
        }
    }
}

/// Aggregated token counters carried on each [`FlowNode`]. Mirrors the
/// fields of [`crate::reader::Usage`] with one normalization: the
/// `cache_create_5m` / `cache_create_1h` TTL split collapses into a
/// single `cache_write` counter, matching the span-tree's locked
/// attribute schema.
///
/// Renamed from the SDK's `Usage` to `TurnTokens` here because the
/// flow-graph context is unambiguously per-node aggregation, and the
/// name `Usage` is overloaded elsewhere in the SDK.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTokens {
    /// Prompt input tokens.
    pub input: u64,
    /// Completion output tokens.
    pub output: u64,
    /// Cached-prefix tokens read.
    pub cache_read: u64,
    /// Sum of `cache_create_5m` + `cache_create_1h`.
    pub cache_write: u64,
    /// Extended-thinking reasoning tokens.
    pub reasoning: u64,
}

impl TurnTokens {
    /// Element-wise sum into `self`.
    pub fn add(&mut self, other: TurnTokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }
}

/// One node in a [`FlowGraph`].
///
/// Coordinates `(x, y)` are emitted directly here so renderers don't
/// reinvent the layout. The layout policy is documented at the module
/// level — render code should treat these as the source of truth and
/// only translate them into render-space (e.g. add a margin offset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowNode {
    /// Stable node identity — see the module-level "Node identity"
    /// section for the format. Two nodes with the same id describe
    /// the same underlying span.
    pub id: String,
    /// What kind of node this is. See [`FlowNodeKind`].
    pub kind: FlowNodeKind,
    /// 0-indexed position of the turn this node belongs to within the
    /// session. Promoted from [`TurnSpanTree::turn_number`] so callers
    /// can group / filter without a second lookup.
    pub turn_number: u32,
    /// Rail index — `0` for the main rail, `1+` for dispatched
    /// subagent rails. Rails are session-scoped; the same subagent
    /// dispatch always lands on a fresh rail index.
    pub rail: u32,
    /// Human-readable label — tool name for `ToolUse`, model name for
    /// `Inference`, agent type for `Subagent`. Renderers may truncate
    /// for display.
    pub label: String,
    /// Model identifier when known (only [`FlowNodeKind::Inference`]
    /// carries this today).
    pub model: Option<String>,
    /// Aggregated token usage attributed to this node. For
    /// [`FlowNodeKind::Inference`] this is the inference's own usage;
    /// for [`FlowNodeKind::Subagent`] it is the sum across the
    /// transcript (currently zero — the subagent's nested transcripts
    /// aren't yet re-parsed inline). `ToolUse` / `Skill` carry zeros.
    pub tokens: TurnTokens,
    /// Wall-clock duration in milliseconds, derived from the span's
    /// `end_ms - start_ms`. `0` when either timestamp is missing.
    pub duration_ms: i64,
    /// OTel-aligned status pulled directly from the source span.
    pub status: SpanStatus,
    /// Layout coordinate — pixel position of the node's column anchor.
    /// See [`INTER_TURN_GAP`] for the column spacing.
    pub x: i32,
    /// Layout coordinate — pixel position of the node's row anchor.
    /// See [`RAIL_GAP`] for the row spacing.
    pub y: i32,
}

/// What kind of edge a [`FlowEdge`] is. Styling is renderer-specific;
/// the source of truth for "what should this edge look like" is the
/// renderer's mapping table, not this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowEdgeKind {
    /// Sequential within a rail.
    Default,
    /// Main rail → subagent rail at the dispatching `Task` `ToolUse`.
    Dispatch,
    /// Subagent rail → main rail at the subagent's terminal node.
    Return,
    /// Sequential within a subagent rail.
    Subagent,
    /// Connects the synthetic turn anchor to an orphan subagent
    /// (a [`SpanKind::Subagent`] with `attributes["unattached"] = true`).
    Unattached,
}

impl FlowEdgeKind {
    /// Kebab-case wire label.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Dispatch => "dispatch",
            Self::Return => "return",
            Self::Subagent => "subagent",
            Self::Unattached => "unattached",
        }
    }
}

/// One edge in a [`FlowGraph`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowEdge {
    /// Source node id (see [`FlowNode::id`]).
    pub from: String,
    /// Destination node id.
    pub to: String,
    /// What kind of edge this is.
    pub kind: FlowEdgeKind,
}

/// Per-session inference-flow DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowGraph {
    /// Session this graph describes.
    pub session_id: String,
    /// Number of turns included in the graph after applying
    /// [`FlowOpts::max_turns`].
    pub turn_count: u32,
    /// Total number of turns in the session before truncation. Equal
    /// to `turn_count` when no truncation happened. Renderers use this
    /// to surface "showing N of M turns" when the cap kicks in.
    pub total_turn_count: u32,
    /// Whether the graph was truncated by [`FlowOpts::max_turns`].
    pub truncated: bool,
    /// All nodes, in causal (turn → rail → in-rail) order.
    pub nodes: Vec<FlowNode>,
    /// All edges. Order is: per-turn sequential edges first, then
    /// dispatch / return edges, then unattached edges.
    pub edges: Vec<FlowEdge>,
}

/// Options for [`crate::LedgerHandle::flow_graph`] / [`flow_graph_from_trees`].
#[derive(Debug, Clone, Default)]
pub struct FlowOpts {
    /// Cap the number of turns rendered. When `None`, defaults to
    /// [`DEFAULT_MAX_TURNS`]. Pass `Some(0)` to disable the cap —
    /// useful for downstream tooling that wants the full graph.
    pub max_turns: Option<u32>,
}

impl FlowOpts {
    /// Resolve the effective max-turn cap. `Some(0)` disables the cap;
    /// `None` falls through to [`DEFAULT_MAX_TURNS`].
    pub fn effective_max_turns(&self) -> Option<u32> {
        match self.max_turns {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(DEFAULT_MAX_TURNS),
        }
    }
}

/// Build a [`FlowGraph`] from a session's pre-built [`TurnSpanTree`]s.
///
/// Pure projection — no DB writes, no caching. Callers that have the
/// trees in hand (tests, downstream embedders) call this directly;
/// callers wanting the convenience of pulling trees from the ledger
/// use [`crate::LedgerHandle::flow_graph`].
///
/// `session_id` is captured on the returned graph; when `trees` is
/// non-empty the first tree's `session_id` will match.
pub fn flow_graph_from_trees(
    session_id: &str,
    trees: &[TurnSpanTree],
    opts: FlowOpts,
) -> FlowGraph {
    build_with_finalize(session_id, trees, opts)
}

/// Internal accumulator that walks each [`TurnSpanTree`] and emits
/// nodes / edges / layout coordinates. Kept private — embedders that
/// want to drive the projection should compose the public free
/// function above instead of poking the builder directly.
#[derive(Default)]
struct Builder {
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    /// Sequential layout edges first; dispatch/return/unattached buffered
    /// so the resulting `FlowGraph::edges` reads cleanly (sequence, then
    /// cross-rail, then orphan).
    cross_rail_edges: Vec<FlowEdge>,
    unattached_edges: Vec<FlowEdge>,
    /// Tracks the highest rail index assigned so far in the whole
    /// session. Rails are session-scoped — every subagent dispatch
    /// gets a fresh rail rather than reusing.
    next_rail: u32,
    /// Tracks the last node id emitted on the main rail across turns,
    /// so a subagent rail's `Return` edge can target the first node
    /// of the *next* main-rail inference.
    last_main_node_per_turn: Vec<(u32, String)>,
}

impl Builder {
    fn add_turn(&mut self, tree: &TurnSpanTree) {
        let turn_x = (tree.turn_number as i32) * INTER_TURN_GAP;
        let main_rail = 0;
        let mut prev_main_id: Option<String> = None;
        let mut inference_index: u32 = 0;
        let mut main_node_y: i32 = 0;

        for child in &tree.root.children {
            match child.kind {
                SpanKind::Inference => {
                    let inf_id = format!("{}:inf-{}", tree.turn_id, inference_index);
                    let node = FlowNode {
                        id: inf_id.clone(),
                        kind: FlowNodeKind::Inference,
                        turn_number: tree.turn_number,
                        rail: main_rail,
                        label: format!("inf #{}", inference_index + 1),
                        model: span_string(child, "model").or_else(|| {
                            // Fall back to the span's display name when
                            // the attribute is missing — Codex builders
                            // skip the explicit attribute for inferences
                            // they synthesized from a TurnRecord.
                            if !child.name.is_empty() {
                                Some(child.name.clone())
                            } else {
                                None
                            }
                        }),
                        tokens: tokens_from_attrs(child),
                        duration_ms: span_duration(child),
                        status: child.status.clone(),
                        x: turn_x,
                        y: main_node_y,
                    };
                    self.nodes.push(node);
                    if let Some(prev) = prev_main_id.replace(inf_id.clone()) {
                        self.edges.push(FlowEdge {
                            from: prev,
                            to: inf_id.clone(),
                            kind: FlowEdgeKind::Default,
                        });
                    }

                    // Tool-use children of this inference — sequential
                    // along the main rail under the inference.
                    let mut prev_tool_id = inf_id.clone();
                    for tool in &child.children {
                        if !matches!(tool.kind, SpanKind::ToolUse | SpanKind::Skill) {
                            continue;
                        }
                        main_node_y += RAIL_GAP;
                        let tool_use_id = span_string(tool, "tool_use_id")
                            .unwrap_or_else(|| format!("nokey-{}", self.nodes.len()));
                        let node_id = format!("{}:tu-{}", tree.turn_id, tool_use_id);
                        let kind = match tool.kind {
                            SpanKind::Skill => FlowNodeKind::Skill,
                            _ => FlowNodeKind::ToolUse,
                        };
                        self.nodes.push(FlowNode {
                            id: node_id.clone(),
                            kind,
                            turn_number: tree.turn_number,
                            rail: main_rail,
                            label: tool.name.clone(),
                            model: None,
                            tokens: TurnTokens::default(),
                            duration_ms: span_duration(tool),
                            status: tool.status.clone(),
                            x: turn_x,
                            y: main_node_y,
                        });
                        self.edges.push(FlowEdge {
                            from: prev_tool_id.clone(),
                            to: node_id.clone(),
                            kind: FlowEdgeKind::Default,
                        });
                        prev_tool_id = node_id.clone();

                        // Dispatched subagents nested under this tool_use.
                        // Each one gets its own rail.
                        for nested in &tool.children {
                            if !matches!(nested.kind, SpanKind::Subagent) {
                                continue;
                            }
                            self.next_rail += 1;
                            let rail = self.next_rail;
                            // The subagent rail inherits its Y from the
                            // dispatching node (the ToolUse) so the
                            // branch point is visually obvious; we then
                            // add a single RAIL_GAP to leave room.
                            let subagent_y = main_node_y + RAIL_GAP;
                            let sub_first_id = self.emit_subagent_rail(
                                tree,
                                nested,
                                rail,
                                turn_x,
                                subagent_y,
                                Some(inference_index),
                            );
                            if let Some(first_id) = sub_first_id {
                                self.cross_rail_edges.push(FlowEdge {
                                    from: node_id.clone(),
                                    to: first_id,
                                    kind: FlowEdgeKind::Dispatch,
                                });
                            }
                        }
                    }
                    prev_main_id = Some(prev_tool_id);
                    inference_index += 1;
                    // Reserve a fresh main-rail row for the next inference.
                    main_node_y += RAIL_GAP;
                }
                SpanKind::Subagent => {
                    // Orphan subagent surfaced under the turn root with
                    // `unattached = true`. Render on its own rail with
                    // an `Unattached` edge from the most recent main-rail
                    // node (or from the previous turn's last main node,
                    // or — when neither exists — leave the rail
                    // anchorless so renderers can highlight it).
                    self.next_rail += 1;
                    let rail = self.next_rail;
                    let unattached_y = main_node_y + RAIL_GAP;
                    // Orphan subagents have no dispatching inference, so
                    // we pass `None` and `emit_subagent_rail` skips the
                    // return-edge buffering entirely (orphans never
                    // "return" to a main-rail successor — they connect
                    // via an `Unattached` edge from the most recent main
                    // node and that's the end of the chain).
                    let first_id = self.emit_subagent_rail(
                        tree,
                        child,
                        rail,
                        turn_x,
                        unattached_y,
                        None,
                    );
                    if let Some(first_id) = first_id {
                        let anchor = prev_main_id.clone().or_else(|| {
                            self.last_main_node_per_turn
                                .last()
                                .map(|(_, id)| id.clone())
                        });
                        if let Some(from) = anchor {
                            self.unattached_edges.push(FlowEdge {
                                from,
                                to: first_id,
                                kind: FlowEdgeKind::Unattached,
                            });
                        }
                    }
                }
                // Other span kinds (UserPrompt, ToolResult, etc.) don't
                // surface as flow nodes — the flow view is the inference
                // / tool / subagent skeleton, not the full trace.
                _ => {}
            }
        }

        if let Some(id) = prev_main_id {
            self.last_main_node_per_turn.push((tree.turn_number, id));
        }
    }

    /// Walk a subagent's children and emit a sequential rail. Returns
    /// the first node id emitted (for the `Dispatch` edge) and pushes
    /// a `Return` edge from the last node back toward whatever main
    /// node comes next in the session. Currently the only meaningful
    /// children we render under a subagent are nested `Inference` /
    /// `ToolUse` spans; sidecar transcripts are not re-parsed inline
    /// (the span tree leaves them as a single node, per #430).
    fn emit_subagent_rail(
        &mut self,
        tree: &TurnSpanTree,
        sub: &SpanNode,
        rail: u32,
        x_anchor: i32,
        y_anchor: i32,
        dispatch_inference_index: Option<u32>,
    ) -> Option<String> {
        let agent_id = span_string(sub, "agent_id")
            .unwrap_or_else(|| format!("rail-{rail}"));
        let label = if !sub.name.is_empty() {
            sub.name.clone()
        } else {
            span_string(sub, "agent_type").unwrap_or_else(|| "subagent".into())
        };
        let root_id = format!("{}:sa-{agent_id}", tree.turn_id);
        let mut y = y_anchor;
        self.nodes.push(FlowNode {
            id: root_id.clone(),
            kind: FlowNodeKind::Subagent,
            turn_number: tree.turn_number,
            rail,
            label,
            model: None,
            tokens: tokens_from_attrs(sub),
            duration_ms: span_duration(sub),
            status: sub.status.clone(),
            x: x_anchor,
            y,
        });

        // Walk nested inferences / tool_uses if the span tree has any.
        // Today these are absent (subagent transcripts ship as opaque
        // nodes) but the projection is written defensively so a future
        // builder that does materialize them lights up automatically.
        let mut prev_id = root_id.clone();
        let mut nested_index: u32 = 0;
        for inner in sub.iter_dfs().skip(1) {
            match inner.kind {
                SpanKind::Inference => {
                    y += RAIL_GAP;
                    let id = format!("{}:sa-{agent_id}:inf-{nested_index}", tree.turn_id);
                    self.nodes.push(FlowNode {
                        id: id.clone(),
                        kind: FlowNodeKind::Inference,
                        turn_number: tree.turn_number,
                        rail,
                        label: format!("inf #{}", nested_index + 1),
                        model: span_string(inner, "model"),
                        tokens: tokens_from_attrs(inner),
                        duration_ms: span_duration(inner),
                        status: inner.status.clone(),
                        x: x_anchor,
                        y,
                    });
                    self.edges.push(FlowEdge {
                        from: prev_id.clone(),
                        to: id.clone(),
                        kind: FlowEdgeKind::Subagent,
                    });
                    prev_id = id;
                    nested_index += 1;
                }
                SpanKind::ToolUse | SpanKind::Skill => {
                    y += RAIL_GAP;
                    let tu_id = span_string(inner, "tool_use_id")
                        .unwrap_or_else(|| format!("nokey-{}", self.nodes.len()));
                    let id = format!("{}:sa-{agent_id}:tu-{tu_id}", tree.turn_id);
                    let kind = match inner.kind {
                        SpanKind::Skill => FlowNodeKind::Skill,
                        _ => FlowNodeKind::ToolUse,
                    };
                    self.nodes.push(FlowNode {
                        id: id.clone(),
                        kind,
                        turn_number: tree.turn_number,
                        rail,
                        label: inner.name.clone(),
                        model: None,
                        tokens: TurnTokens::default(),
                        duration_ms: span_duration(inner),
                        status: inner.status.clone(),
                        x: x_anchor,
                        y,
                    });
                    self.edges.push(FlowEdge {
                        from: prev_id.clone(),
                        to: id.clone(),
                        kind: FlowEdgeKind::Subagent,
                    });
                    prev_id = id;
                }
                _ => {}
            }
        }

        // Buffer a Return edge to wire back to the *chronologically
        // next* main-rail inference after the dispatch point. Encode
        // both the turn and the dispatching inference's index in the
        // placeholder so `finalize_returns` can pick the right
        // successor — including when a turn contains multiple main
        // inferences and the dispatch happened from a non-last one
        // (in which case the return should land in the same turn,
        // not skip to turn+1).
        //
        // Orphan rails (`dispatch_inference_index == None`) don't get
        // a return edge — they have no caller to return to and the
        // `Unattached` edge already documents the relationship.
        if let Some(idx) = dispatch_inference_index {
            self.cross_rail_edges.push(FlowEdge {
                from: prev_id,
                to: format!("__return_anchor:{}:{}", tree.turn_number, idx),
                kind: FlowEdgeKind::Return,
            });
        }

        Some(root_id)
    }
}

impl Builder {
    /// Resolve the `__return_anchor:<turn>:<inference_idx>` placeholders
    /// into the chronologically next main-rail inference id, dropping
    /// the edge when no downstream main inference exists (dispatch was
    /// the last inference of the session).
    ///
    /// Important: the lookup must consider same-turn successors. A turn
    /// can carry multiple main-rail inferences (e.g. an assistant turn
    /// that dispatches a subagent mid-turn, then continues with another
    /// inference); when that happens the return edge needs to land on
    /// that next same-turn inference, not on the first inference of the
    /// following turn.
    fn finalize_returns(&mut self) {
        // Build the main-rail inference timeline in emission order. The
        // builder pushes main-rail inferences `(turn_number, idx)` in
        // strictly increasing order, so iterating `self.nodes` already
        // gives us the right sequence — we just have to filter and
        // remember the per-turn index that was baked into each id.
        //
        // Each main-rail inference id is `{turn_id}:inf-{idx}`; we strip
        // the `:inf-` suffix to recover `idx` rather than re-parsing the
        // attribute payload.
        let mut timeline: Vec<(u32, u32, String)> = Vec::new();
        for node in &self.nodes {
            if node.rail != 0 || !matches!(node.kind, FlowNodeKind::Inference) {
                continue;
            }
            let Some(sep) = node.id.rfind(":inf-") else {
                continue;
            };
            let suffix = &node.id[sep + ":inf-".len()..];
            let Ok(idx) = suffix.parse::<u32>() else {
                continue;
            };
            timeline.push((node.turn_number, idx, node.id.clone()));
        }
        // Defensive: the emission order should already be sorted, but
        // pin it explicitly so a future builder reorder doesn't quietly
        // break return resolution.
        timeline.sort_by_key(|(t, i, _)| (*t, *i));

        let mut resolved = Vec::with_capacity(self.cross_rail_edges.len());
        for edge in self.cross_rail_edges.drain(..) {
            if let Some(rest) = edge.to.strip_prefix("__return_anchor:") {
                // New format: `{turn}:{inference_idx}`. Anything else is
                // a stale placeholder we leave on the side (it won't
                // resolve and gets dropped, matching prior behavior on
                // unparseable anchors).
                let parsed = rest
                    .split_once(':')
                    .and_then(|(t, i)| Some((t.parse::<u32>().ok()?, i.parse::<u32>().ok()?)));
                if let Some((from_turn, from_idx)) = parsed {
                    // First timeline entry strictly after `(from_turn,
                    // from_idx)`. `partition_point` gives us the
                    // insertion index immediately past the matching
                    // dispatch entry, which is exactly the successor.
                    let pivot = timeline.partition_point(|(t, i, _)| {
                        (*t, *i) <= (from_turn, from_idx)
                    });
                    if let Some((_, _, id)) = timeline.get(pivot) {
                        resolved.push(FlowEdge {
                            from: edge.from,
                            to: id.clone(),
                            kind: FlowEdgeKind::Return,
                        });
                    }
                    // No downstream main inference — drop the edge silently.
                }
            } else {
                resolved.push(edge);
            }
        }
        self.cross_rail_edges = resolved;
    }
}

/// Pull a `String` attribute off a span without panicking when the key
/// is missing or carries a non-string value.
fn span_string(node: &SpanNode, key: &str) -> Option<String> {
    match node.attributes.get(key) {
        Some(AttrValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Project a span's token attributes into a [`TurnTokens`]. Missing
/// keys default to `0` — matches [`TurnSpanTree::sum_attr_int`]'s
/// silent-fallback contract.
fn tokens_from_attrs(node: &SpanNode) -> TurnTokens {
    TurnTokens {
        input: attr_int(node, "tokens.input"),
        output: attr_int(node, "tokens.output"),
        cache_read: attr_int(node, "tokens.cache_read"),
        cache_write: attr_int(node, "tokens.cache_write"),
        reasoning: attr_int(node, "tokens.reasoning"),
    }
}

fn attr_int(node: &SpanNode, key: &str) -> u64 {
    match node.attributes.get(key) {
        Some(AttrValue::Int(v)) => u64::try_from(*v).unwrap_or(0),
        _ => 0,
    }
}

fn span_duration(node: &SpanNode) -> i64 {
    if node.end_ms <= 0 || node.start_ms <= 0 {
        return 0;
    }
    (node.end_ms - node.start_ms).max(0)
}

// --- builder driver --------------------------------------------------------

/// Internal driver shared by [`flow_graph_from_trees`] and
/// [`crate::LedgerHandle::flow_graph`]. Walks each turn, finalizes the
/// buffered cross-rail / unattached edges, and assembles the final
/// `FlowGraph` value. Kept private — callers go through one of the
/// public entrypoints so the projection contract has one home.
fn build_with_finalize(
    session_id: &str,
    trees: &[TurnSpanTree],
    opts: FlowOpts,
) -> FlowGraph {
    let total_turn_count = u32::try_from(trees.len()).unwrap_or(u32::MAX);
    let max_turns = opts.effective_max_turns();
    let take = match max_turns {
        Some(cap) => (cap as usize).min(trees.len()),
        None => trees.len(),
    };
    let trees = &trees[..take];
    let turn_count = u32::try_from(trees.len()).unwrap_or(u32::MAX);
    let truncated = turn_count < total_turn_count;

    let mut builder = Builder::default();
    for tree in trees {
        builder.add_turn(tree);
    }
    builder.finalize_returns();
    let Builder {
        nodes,
        mut edges,
        cross_rail_edges,
        unattached_edges,
        ..
    } = builder;
    edges.extend(cross_rail_edges);
    edges.extend(unattached_edges);

    FlowGraph {
        session_id: session_id.to_string(),
        turn_count,
        total_turn_count,
        truncated,
        nodes,
        edges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::span_tree::{SpanKind, SpanNode, SpanStatus};

    fn make_tree(turn_number: u32, root: SpanNode) -> TurnSpanTree {
        TurnSpanTree {
            session_id: "sess-1".into(),
            turn_id: format!("msg-{turn_number}"),
            turn_number,
            root,
        }
    }

    fn inference(model: &str, request_id: &str) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::Inference, model);
        n.set_attr("model", AttrValue::str(model));
        n.set_attr("request_id", AttrValue::str(request_id));
        n.set_attr("tokens.input", AttrValue::Int(100));
        n.set_attr("tokens.output", AttrValue::Int(20));
        n.set_attr("tokens.cache_read", AttrValue::Int(500));
        n.set_attr("tokens.cache_write", AttrValue::Int(0));
        n.set_attr("tokens.reasoning", AttrValue::Int(0));
        n.start_ms = 1_000;
        n.end_ms = 1_500;
        n
    }

    fn tool_use(name: &str, id: &str) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::ToolUse, name);
        n.set_attr("tool_use_id", AttrValue::str(id));
        n
    }

    fn subagent(agent_id: &str, unattached: bool) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::Subagent, "reviewer");
        n.set_attr("agent_id", AttrValue::str(agent_id));
        n.set_attr("agent_type", AttrValue::str("reviewer"));
        if unattached {
            n.set_attr("unattached", AttrValue::Bool(true));
        }
        n
    }

    fn turn_root() -> SpanNode {
        SpanNode::new(SpanKind::Turn, "turn")
    }

    #[test]
    fn empty_trees_produce_empty_graph() {
        let graph = flow_graph_from_trees("sess-1", &[], FlowOpts::default());
        assert_eq!(graph.nodes.len(), 0);
        assert_eq!(graph.edges.len(), 0);
        assert_eq!(graph.turn_count, 0);
        assert_eq!(graph.total_turn_count, 0);
        assert!(!graph.truncated);
    }

    #[test]
    fn single_inference_turn_emits_one_node_zero_edges() {
        let mut root = turn_root();
        root.children.push(inference("claude-sonnet", "req-1"));
        let graph = flow_graph_from_trees("sess-1", &[make_tree(0, root)], FlowOpts::default());
        assert_eq!(graph.nodes.len(), 1);
        let node = &graph.nodes[0];
        assert_eq!(node.kind, FlowNodeKind::Inference);
        assert_eq!(node.id, "msg-0:inf-0");
        assert_eq!(node.rail, 0);
        assert_eq!(node.turn_number, 0);
        assert_eq!(node.x, 0);
        assert_eq!(node.y, 0);
        assert_eq!(node.tokens.input, 100);
        assert_eq!(node.tokens.output, 20);
        assert_eq!(node.duration_ms, 500);
        assert_eq!(graph.edges.len(), 0);
    }

    #[test]
    fn two_inferences_in_a_turn_are_connected_by_default_edge() {
        let mut root = turn_root();
        root.children.push(inference("claude-sonnet", "req-1"));
        root.children.push(inference("claude-sonnet", "req-2"));
        let graph = flow_graph_from_trees("sess-1", &[make_tree(0, root)], FlowOpts::default());
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].kind, FlowEdgeKind::Default);
        assert_eq!(graph.edges[0].from, "msg-0:inf-0");
        assert_eq!(graph.edges[0].to, "msg-0:inf-1");
    }

    #[test]
    fn tool_use_under_inference_emits_node_and_default_edge() {
        let mut inf = inference("claude-sonnet", "req-1");
        inf.children.push(tool_use("Bash", "tu-a"));
        let mut root = turn_root();
        root.children.push(inf);
        let graph = flow_graph_from_trees("sess-1", &[make_tree(0, root)], FlowOpts::default());
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[1].kind, FlowNodeKind::ToolUse);
        assert_eq!(graph.nodes[1].id, "msg-0:tu-tu-a");
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].kind, FlowEdgeKind::Default);
        assert_eq!(graph.edges[0].from, "msg-0:inf-0");
        assert_eq!(graph.edges[0].to, "msg-0:tu-tu-a");
    }

    #[test]
    fn task_dispatch_emits_subagent_rail_with_dispatch_edge() {
        let mut task = tool_use("Task", "tu-task");
        task.children.push(subagent("agent-1", false));
        let mut inf = inference("claude-sonnet", "req-1");
        inf.children.push(task);
        let mut root = turn_root();
        root.children.push(inf);
        let graph = flow_graph_from_trees("sess-1", &[make_tree(0, root)], FlowOpts::default());
        // inference + tool_use + subagent = 3 nodes.
        assert_eq!(graph.nodes.len(), 3);
        let sub = graph
            .nodes
            .iter()
            .find(|n| n.kind == FlowNodeKind::Subagent)
            .expect("subagent node missing");
        assert_eq!(sub.rail, 1, "first subagent rail should be 1");
        // The subagent rail inherits its Y from the dispatching tool_use
        // (which is one RAIL_GAP below the inference) and offsets by
        // another RAIL_GAP for the rail's first row.
        assert_eq!(sub.y, RAIL_GAP * 2);
        // Edges: inf -> tool_use (Default), tool_use -> subagent (Dispatch).
        // No Return edge because there is no next turn.
        let kinds: Vec<FlowEdgeKind> = graph.edges.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&FlowEdgeKind::Default));
        assert!(kinds.contains(&FlowEdgeKind::Dispatch));
        assert!(!kinds.contains(&FlowEdgeKind::Return));
    }

    #[test]
    fn task_dispatch_with_following_turn_emits_return_edge() {
        let mut task = tool_use("Task", "tu-task");
        task.children.push(subagent("agent-1", false));
        let mut inf = inference("claude-sonnet", "req-1");
        inf.children.push(task);
        let mut root0 = turn_root();
        root0.children.push(inf);

        let mut root1 = turn_root();
        root1.children.push(inference("claude-sonnet", "req-2"));

        let graph = flow_graph_from_trees(
            "sess-1",
            &[make_tree(0, root0), make_tree(1, root1)],
            FlowOpts::default(),
        );
        let kinds: Vec<FlowEdgeKind> = graph.edges.iter().map(|e| e.kind).collect();
        assert!(
            kinds.contains(&FlowEdgeKind::Return),
            "expected a Return edge, got {:?}",
            kinds
        );
        let return_edge = graph
            .edges
            .iter()
            .find(|e| e.kind == FlowEdgeKind::Return)
            .unwrap();
        assert_eq!(return_edge.to, "msg-1:inf-0");
    }

    #[test]
    fn return_edge_resolves_to_same_turn_next_inference() {
        // Setup: a single turn with two main-rail inferences. The first
        // inference dispatches a subagent; the return edge must land on
        // the second same-turn inference, not on the next turn (the
        // pre-fix code only searched `turn_number + 1..`, which would
        // either drop the edge or skip past the same-turn successor).
        let mut task = tool_use("Task", "tu-task");
        task.children.push(subagent("agent-1", false));
        let mut inf0 = inference("claude-sonnet", "req-1");
        inf0.children.push(task);
        let inf1 = inference("claude-sonnet", "req-2");
        let mut root = turn_root();
        root.children.push(inf0);
        root.children.push(inf1);

        let graph = flow_graph_from_trees(
            "sess-1",
            &[make_tree(0, root)],
            FlowOpts::default(),
        );

        let return_edge = graph
            .edges
            .iter()
            .find(|e| e.kind == FlowEdgeKind::Return)
            .expect("expected a Return edge for the same-turn dispatch");
        assert_eq!(
            return_edge.to, "msg-0:inf-1",
            "return should land on the chronologically next main inference \
             (msg-0:inf-1), not skip to a later turn"
        );
    }

    #[test]
    fn return_edge_resolves_to_next_turn_when_dispatch_is_last_inference_of_turn() {
        // Regression guard for the prior behavior: when the dispatch
        // inference *is* the last one in its turn, the return must
        // still cross into the next turn's first main inference.
        let mut task = tool_use("Task", "tu-task");
        task.children.push(subagent("agent-1", false));
        let mut inf = inference("claude-sonnet", "req-1");
        inf.children.push(task);
        let mut root0 = turn_root();
        root0.children.push(inf);

        let mut root1 = turn_root();
        root1.children.push(inference("claude-sonnet", "req-2"));
        root1.children.push(inference("claude-sonnet", "req-3"));

        let graph = flow_graph_from_trees(
            "sess-1",
            &[make_tree(0, root0), make_tree(1, root1)],
            FlowOpts::default(),
        );

        let return_edge = graph
            .edges
            .iter()
            .find(|e| e.kind == FlowEdgeKind::Return)
            .expect("expected a Return edge crossing into turn 1");
        assert_eq!(return_edge.to, "msg-1:inf-0");
    }

    #[test]
    fn return_edge_dropped_when_dispatch_is_last_inference_of_session() {
        // Session ends with a subagent dispatch from the last main
        // inference. There is no successor to return to, so the edge
        // must be dropped silently rather than left pointing at a
        // placeholder id (which would break Mermaid/SVG renderers).
        let mut task = tool_use("Task", "tu-task");
        task.children.push(subagent("agent-1", false));
        let mut inf = inference("claude-sonnet", "req-1");
        inf.children.push(task);
        let mut root = turn_root();
        root.children.push(inf);
        let graph = flow_graph_from_trees(
            "sess-1",
            &[make_tree(0, root)],
            FlowOpts::default(),
        );
        assert!(
            !graph.edges.iter().any(|e| e.kind == FlowEdgeKind::Return),
            "no Return edge expected when dispatch is the terminal inference"
        );
        assert!(
            !graph.edges.iter().any(|e| e.to.starts_with("__return_anchor")),
            "no unresolved placeholders should leak into the final graph"
        );
    }

    #[test]
    fn orphan_subagent_emits_unattached_edge() {
        let mut root = turn_root();
        root.children.push(inference("claude-sonnet", "req-1"));
        root.children.push(subagent("agent-orphan", true));
        let graph = flow_graph_from_trees("sess-1", &[make_tree(0, root)], FlowOpts::default());
        let unattached = graph
            .edges
            .iter()
            .filter(|e| e.kind == FlowEdgeKind::Unattached)
            .count();
        assert_eq!(unattached, 1, "exactly one Unattached edge expected");
        let edge = graph
            .edges
            .iter()
            .find(|e| e.kind == FlowEdgeKind::Unattached)
            .unwrap();
        assert_eq!(edge.from, "msg-0:inf-0");
        assert!(edge.to.contains(":sa-agent-orphan"));
    }

    #[test]
    fn turn_columns_use_inter_turn_gap_spacing() {
        let mut root0 = turn_root();
        root0.children.push(inference("claude-sonnet", "req-1"));
        let mut root1 = turn_root();
        root1.children.push(inference("claude-sonnet", "req-2"));
        let graph = flow_graph_from_trees(
            "sess-1",
            &[make_tree(0, root0), make_tree(1, root1)],
            FlowOpts::default(),
        );
        let n0 = graph.nodes.iter().find(|n| n.turn_number == 0).unwrap();
        let n1 = graph.nodes.iter().find(|n| n.turn_number == 1).unwrap();
        assert_eq!(n0.x, 0);
        assert_eq!(n1.x, INTER_TURN_GAP);
    }

    #[test]
    fn max_turns_truncates_and_flags_truncated() {
        let trees: Vec<TurnSpanTree> = (0..10)
            .map(|i| {
                let mut root = turn_root();
                root.children.push(inference("claude-sonnet", "r"));
                make_tree(i, root)
            })
            .collect();
        let graph = flow_graph_from_trees(
            "sess-1",
            &trees,
            FlowOpts { max_turns: Some(3) },
        );
        assert_eq!(graph.turn_count, 3);
        assert_eq!(graph.total_turn_count, 10);
        assert!(graph.truncated);
        assert_eq!(graph.nodes.iter().filter(|n| n.rail == 0).count(), 3);
    }

    #[test]
    fn max_turns_zero_disables_cap() {
        let trees: Vec<TurnSpanTree> = (0..3)
            .map(|i| {
                let mut root = turn_root();
                root.children.push(inference("claude-sonnet", "r"));
                make_tree(i, root)
            })
            .collect();
        let graph = flow_graph_from_trees(
            "sess-1",
            &trees,
            FlowOpts { max_turns: Some(0) },
        );
        assert_eq!(graph.turn_count, 3);
        assert!(!graph.truncated);
    }

    #[test]
    fn flow_node_kind_round_trips() {
        for k in [
            FlowNodeKind::Inference,
            FlowNodeKind::ToolUse,
            FlowNodeKind::Subagent,
            FlowNodeKind::Skill,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: FlowNodeKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn flow_edge_kind_round_trips() {
        for k in [
            FlowEdgeKind::Default,
            FlowEdgeKind::Dispatch,
            FlowEdgeKind::Return,
            FlowEdgeKind::Subagent,
            FlowEdgeKind::Unattached,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: FlowEdgeKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn flow_graph_camel_case_fields() {
        let graph = FlowGraph {
            session_id: "s".into(),
            turn_count: 1,
            total_turn_count: 1,
            truncated: false,
            nodes: vec![FlowNode {
                id: "n".into(),
                kind: FlowNodeKind::Inference,
                turn_number: 0,
                rail: 0,
                label: "inf #1".into(),
                model: Some("claude".into()),
                tokens: TurnTokens::default(),
                duration_ms: 0,
                status: SpanStatus::Ok,
                x: 0,
                y: 0,
            }],
            edges: vec![],
        };
        let s = serde_json::to_string(&graph).unwrap();
        assert!(s.contains("\"sessionId\":\"s\""), "got {s}");
        assert!(s.contains("\"turnCount\":1"), "got {s}");
        assert!(s.contains("\"totalTurnCount\":1"), "got {s}");
        assert!(s.contains("\"turnNumber\":0"), "got {s}");
        assert!(s.contains("\"durationMs\":0"), "got {s}");
        assert!(s.contains("\"cacheRead\":0"), "got {s}");
        assert!(s.contains("\"cacheWrite\":0"), "got {s}");
    }
}
