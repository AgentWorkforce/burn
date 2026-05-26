//! `burn flow --session <id>` — render a session's inference-flow DAG.
//!
//! Thin presenter over [`relayburn_sdk::LedgerHandle::flow_graph`]. The
//! SDK owns the projection + layout; this module owns three renderers
//! over the resulting [`FlowGraph`]:
//!
//! - **Mermaid** (default human output) — a `graph LR` block to stdout,
//!   suitable for pasting into a PR description or markdown notes.
//! - **SVG** (`--output flow.svg`) — a static SVG with one bar per node
//!   and styled edges per [`FlowEdgeKind`]. No interactivity.
//! - **JSON** (`--json`) — the raw [`FlowGraph`] for downstream tooling.
//!
//! Layout decisions (column spacing, rail spacing, branch geometry)
//! live in the SDK module, not here. The renderer only translates
//! `(x, y)` to render-space — adds a margin offset, picks colors per
//! [`FlowNodeKind`], chooses dash patterns per [`FlowEdgeKind`].
//!
//! ## Output routing matrix
//!
//! | `--json` | `--mermaid` | `--output` | Behavior                                     |
//! |----------|-------------|------------|----------------------------------------------|
//! | yes      | (ignored)   | (ignored)  | JSON to stdout                               |
//! | no       | yes         | None       | Mermaid to stdout                            |
//! | no       | yes         | Some(path) | SVG to path, Mermaid to stdout               |
//! | no       | no          | Some(path) | SVG to path (stdout silent)                  |
//! | no       | no          | None       | Mermaid to stdout (default human output)     |
//!
//! ## SVG byte stability
//!
//! The SVG output is deliberately hand-rolled (no `svg` crate) so the
//! emitted bytes are deterministic for a given input — required by the
//! golden snapshot under `tests/fixtures/cli-golden`. Attribute order,
//! whitespace, and number formatting are all fixed by this module.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use relayburn_sdk::{
    FlowEdge, FlowEdgeKind, FlowGraph, FlowNode, FlowNodeKind, FlowOpts, Ledger, LedgerOpenOptions,
    INTER_TURN_GAP, RAIL_GAP,
};

use crate::cli::{FlowArgs, GlobalArgs};
use crate::render::error::report_error;
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;

/// Outer margin around the laid-out content, in SVG user units.
const SVG_MARGIN: i32 = 24;
/// Width of a node bar in user units. Chosen so labels up to ~10
/// characters fit without truncation at the default font size.
const NODE_WIDTH: i32 = 88;
/// Height of a node bar in user units. Slightly shorter than
/// [`RAIL_GAP`] so vertical edges between stacked nodes have visible
/// space — otherwise the bar's bottom edge coincides with the next
/// node's top edge and the connector renders as a zero-length line.
const NODE_HEIGHT: i32 = RAIL_GAP - 8;
/// Height reserved for the legend block at the top of the SVG.
const LEGEND_HEIGHT: i32 = 56;
/// Horizontal step between legend swatches, in user units. Matches
/// the loop step inside [`render_svg_legend`] — keep these in sync.
const LEGEND_ITEM_STEP: i32 = 96;
/// Number of items on the widest legend row (row 2: default,
/// dispatch, return, subagent, unattached). Used to derive a
/// minimum SVG width so the legend never clips on small graphs.
const LEGEND_ROW2_ITEMS: i32 = 5;
/// Trailing-text reserve after the last legend swatch — the
/// "unattached" label rendered at 11px is roughly 64 user units
/// wide; round up to 80 to keep a small inset before the right
/// margin.
const LEGEND_LAST_LABEL_RESERVE: i32 = 80;
/// Minimum SVG width required to render the legend without
/// clipping. Derived from the layout so any change to the legend
/// (item count, step, label width) propagates automatically:
///
/// `SVG_MARGIN + (N-1) * LEGEND_ITEM_STEP + 18 (text offset) +
///  LEGEND_LAST_LABEL_RESERVE + SVG_MARGIN`
const LEGEND_MIN_WIDTH: i32 = SVG_MARGIN
    + (LEGEND_ROW2_ITEMS - 1) * LEGEND_ITEM_STEP
    + 18
    + LEGEND_LAST_LABEL_RESERVE
    + SVG_MARGIN;

pub fn run(globals: &GlobalArgs, args: FlowArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: FlowArgs) -> anyhow::Result<i32> {
    let progress = TaskProgress::new(globals, "flow");

    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let handle = Ledger::open(opts).inspect_err(|_| {
        progress.finish_and_clear();
    })?;

    progress.set_task("building flow graph");
    let flow_opts = FlowOpts {
        max_turns: args.max_turns,
    };
    let graph = handle
        .flow_graph(&args.session, flow_opts)
        .inspect_err(|_| {
            progress.finish_and_clear();
        })?;
    progress.finish_and_clear();

    // JSON mode is single-output (stdout only) and overrides everything
    // else — the same convention every other read-path verb uses.
    if globals.json {
        render_json(&graph)?;
        return Ok(0);
    }

    // SVG to file when `--output` is set; the human-mode default is
    // Mermaid to stdout.
    if let Some(path) = args.output.as_deref() {
        let svg = render_svg(&graph);
        write_atomic(path, svg.as_bytes())?;
    }

    // Stdout output: Mermaid when (a) no `--output`, or (b) `--mermaid`
    // was passed explicitly to layer it on top of an SVG render.
    if args.output.is_none() || args.mermaid {
        let mermaid = render_mermaid(&graph);
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(mermaid.as_bytes())?;
        handle.flush()?;
    }

    Ok(0)
}

/// Write to `path` via a sibling tmp file + rename so a partially
/// written SVG never lands on disk. Keeps the CLI well-behaved when
/// the user's filesystem fills mid-write or the process gets
/// interrupted.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = match path.file_name() {
        Some(name) => {
            let mut tmp_name = std::ffi::OsString::from(".");
            tmp_name.push(name);
            tmp_name.push(".tmp");
            path.with_file_name(tmp_name)
        }
        None => path.with_extension("tmp"),
    };
    fs::write(&tmp, bytes)?;
    // Windows `rename` fails when the destination already exists; POSIX
    // overwrites in place. Drop the destination first on Windows so a
    // repeat `burn flow --output flow.svg` doesn't trip on the second
    // run. The gate is `cfg!()` rather than `#[cfg]` so the branch
    // still type-checks on Linux/macOS — `path.exists()` is a no-op on
    // the false branch and the dead-code optimizer drops it on
    // non-Windows targets.
    if cfg!(windows) && path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Mermaid renderer
// ---------------------------------------------------------------------------

/// Render the graph as a Mermaid `graph LR` block. Mermaid's auto-layout
/// will pick its own geometry — we can't reproduce the SDK's rail
/// layout exactly — but per-edge styling carries through via class
/// definitions so dispatch / return / unattached edges stay visually
/// distinct.
fn render_mermaid(graph: &FlowGraph) -> String {
    let mut out = String::new();
    out.push_str("```mermaid\n");
    out.push_str("graph LR\n");
    // Mermaid class defs for the four edge styles. Stable ordering for
    // snapshot byte-equality.
    out.push_str("    classDef inference fill:#e3f2fd,stroke:#1565c0,color:#0d47a1;\n");
    out.push_str("    classDef toolUse fill:#fff3e0,stroke:#ef6c00,color:#e65100;\n");
    out.push_str("    classDef subagent fill:#f3e5f5,stroke:#6a1b9a,color:#4a148c;\n");
    out.push_str("    classDef skill fill:#e8f5e9,stroke:#2e7d32,color:#1b5e20;\n");

    // Nodes, in emission order — Mermaid is permissive about node
    // declarations interleaved with edges, but keeping nodes first
    // makes the block much easier to scan.
    for node in &graph.nodes {
        let safe_id = mermaid_id(&node.id);
        let label = mermaid_label(&node.label, node.turn_number);
        writeln!(
            out,
            "    {safe_id}[\"{label}\"]:::{}",
            mermaid_class(node.kind)
        )
        .unwrap();
    }

    if !graph.edges.is_empty() {
        out.push('\n');
    }
    for edge in &graph.edges {
        let from = mermaid_id(&edge.from);
        let to = mermaid_id(&edge.to);
        let arrow = mermaid_arrow(edge.kind);
        writeln!(out, "    {from} {arrow} {to}").unwrap();
    }

    out.push_str("```\n");
    out
}

/// Mermaid node ids must be alphanumeric + underscores. Replace
/// everything else with `_` so our `"{turn_id}:inf-N"` ids parse.
fn mermaid_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Escape user-controlled label text for safe embedding inside
/// Mermaid's `["..."]` label brackets. Quotes get backslash-escaped;
/// newlines and angle brackets get stripped (Mermaid renders them
/// as raw characters which can break parsing).
fn mermaid_label(label: &str, turn_number: u32) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| !matches!(*c, '\n' | '\r' | '<' | '>' | '"'))
        .collect();
    format!("T{turn_number}: {cleaned}")
}

fn mermaid_class(kind: FlowNodeKind) -> &'static str {
    match kind {
        FlowNodeKind::Inference => "inference",
        FlowNodeKind::ToolUse => "toolUse",
        FlowNodeKind::Subagent => "subagent",
        FlowNodeKind::Skill => "skill",
    }
}

fn mermaid_arrow(kind: FlowEdgeKind) -> &'static str {
    match kind {
        FlowEdgeKind::Default => "-->",
        FlowEdgeKind::Subagent => "-->",
        FlowEdgeKind::Dispatch => "-.->|dispatch|",
        FlowEdgeKind::Return => "-.->|return|",
        FlowEdgeKind::Unattached => "-.->|unattached|",
    }
}

// ---------------------------------------------------------------------------
// SVG renderer
// ---------------------------------------------------------------------------

/// Render the graph as a static SVG. Layout coordinates come straight
/// from the SDK — we add a margin offset and reserve the top
/// [`LEGEND_HEIGHT`] units for the legend block. No interactivity,
/// no script tag, no external font references — the output is
/// self-contained and renderable in any SVG viewer.
fn render_svg(graph: &FlowGraph) -> String {
    let (max_x, max_y) = graph.nodes.iter().fold((0_i32, 0_i32), |(mx, my), n| {
        (mx.max(n.x + NODE_WIDTH), my.max(n.y + NODE_HEIGHT))
    });
    // Honor both the node bounds and the legend's intrinsic minimum
    // width — otherwise small graphs (e.g. 2-3 turn sessions) clip the
    // legend on the right. `LEGEND_MIN_WIDTH` is derived from the
    // legend layout constants so the two stay in sync.
    let width = (max_x + SVG_MARGIN * 2).max(LEGEND_MIN_WIDTH);
    let height = max_y + LEGEND_HEIGHT + SVG_MARGIN * 2;

    let mut out = String::with_capacity(4096);
    writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" font-family=\"-apple-system,Segoe UI,Helvetica,sans-serif\" font-size=\"11\">"
    )
    .unwrap();

    // Title
    writeln!(
        out,
        "  <title>burn flow — session {} ({} of {} turns)</title>",
        escape_xml(&graph.session_id),
        graph.turn_count,
        graph.total_turn_count
    )
    .unwrap();

    // Legend
    render_svg_legend(&mut out);

    // Edges first (so node bars draw over them)
    out.push_str("  <g id=\"edges\">\n");
    for edge in &graph.edges {
        if let Some((from, to)) = lookup_edge_endpoints(graph, edge) {
            render_svg_edge(&mut out, edge, from, to);
        }
    }
    out.push_str("  </g>\n");

    // Nodes
    out.push_str("  <g id=\"nodes\">\n");
    for node in &graph.nodes {
        render_svg_node(&mut out, node);
    }
    out.push_str("  </g>\n");

    // Notes block — turn count, truncated flag
    if graph.truncated {
        writeln!(
            out,
            "  <text x=\"{x}\" y=\"{y}\" fill=\"#b71c1c\">showing {n} of {total} turns (capped by --max-turns)</text>",
            x = SVG_MARGIN,
            y = height - 6,
            n = graph.turn_count,
            total = graph.total_turn_count,
        )
        .unwrap();
    }

    out.push_str("</svg>\n");
    out
}

fn render_svg_legend(out: &mut String) {
    let y = SVG_MARGIN;
    let mut x = SVG_MARGIN;
    out.push_str("  <g id=\"legend\">\n");
    for (label, fill, stroke) in [
        ("inference", "#e3f2fd", "#1565c0"),
        ("tool-use", "#fff3e0", "#ef6c00"),
        ("subagent", "#f3e5f5", "#6a1b9a"),
        ("skill", "#e8f5e9", "#2e7d32"),
    ] {
        writeln!(
            out,
            "    <rect x=\"{x}\" y=\"{y}\" width=\"14\" height=\"14\" fill=\"{fill}\" stroke=\"{stroke}\"/>"
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{}\" y=\"{}\" fill=\"#212121\">{}</text>",
            x + 18,
            y + 11,
            label
        )
        .unwrap();
        x += LEGEND_ITEM_STEP;
    }
    // Edge legend on row 2
    let y2 = y + 22;
    let mut x2 = SVG_MARGIN;
    for (label, stroke, dash) in [
        ("default", "#9e9e9e", ""),
        ("dispatch", "#ef6c00", "4 3"),
        ("return", "#ef6c00", "4 3"),
        ("subagent", "#1565c0", ""),
        ("unattached", "#b71c1c", "2 2"),
    ] {
        let dash_attr = if dash.is_empty() {
            String::new()
        } else {
            format!(" stroke-dasharray=\"{dash}\"")
        };
        writeln!(
            out,
            "    <line x1=\"{x2}\" y1=\"{y}\" x2=\"{xe}\" y2=\"{y}\" stroke=\"{stroke}\" stroke-width=\"2\"{dash_attr}/>",
            x2 = x2,
            xe = x2 + 14,
            y = y2 + 7,
        )
        .unwrap();
        writeln!(
            out,
            "    <text x=\"{}\" y=\"{}\" fill=\"#212121\">{}</text>",
            x2 + 18,
            y2 + 11,
            label
        )
        .unwrap();
        x2 += LEGEND_ITEM_STEP;
    }
    out.push_str("  </g>\n");
}

fn render_svg_node(out: &mut String, node: &FlowNode) {
    let (fill, stroke) = node_palette(node.kind);
    let x = node.x + SVG_MARGIN;
    let y = node.y + SVG_MARGIN + LEGEND_HEIGHT;
    let label = svg_truncate(&node.label, 12);
    writeln!(
        out,
        "    <g><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{fill}\" stroke=\"{stroke}\" rx=\"3\"/>\
<text x=\"{tx}\" y=\"{ty}\" fill=\"#212121\">T{tn} {label}</text></g>",
        w = NODE_WIDTH,
        h = NODE_HEIGHT,
        tx = x + 4,
        ty = y + 14,
        tn = node.turn_number,
        label = escape_xml(&label),
    )
    .unwrap();
}

fn render_svg_edge(out: &mut String, edge: &FlowEdge, from: &FlowNode, to: &FlowNode) {
    let (stroke, dash) = edge_palette(edge.kind);
    let dash_attr = if dash.is_empty() {
        String::new()
    } else {
        format!(" stroke-dasharray=\"{dash}\"")
    };
    // Anchor choice: when the two nodes share a column, the edge runs
    // top-to-bottom (bottom-center of `from` → top-center of `to`).
    // Otherwise it runs left-to-right (right-center of `from` →
    // left-center of `to`). Same-column edges happen for sequential
    // tool_uses under an inference and for in-rail subagent steps;
    // cross-column edges happen for sequential turns and dispatch /
    // return wires between rails.
    let same_col = from.x == to.x;
    let (x1, y1, x2, y2) = if same_col {
        let cx = from.x + SVG_MARGIN + NODE_WIDTH / 2;
        let y1 = from.y + SVG_MARGIN + LEGEND_HEIGHT + NODE_HEIGHT;
        let y2 = to.y + SVG_MARGIN + LEGEND_HEIGHT;
        (cx, y1, cx, y2)
    } else {
        let x1 = from.x + SVG_MARGIN + NODE_WIDTH;
        let y1 = from.y + SVG_MARGIN + LEGEND_HEIGHT + NODE_HEIGHT / 2;
        let x2 = to.x + SVG_MARGIN;
        let y2 = to.y + SVG_MARGIN + LEGEND_HEIGHT + NODE_HEIGHT / 2;
        (x1, y1, x2, y2)
    };
    writeln!(
        out,
        "    <line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" stroke=\"{stroke}\" stroke-width=\"1.5\"{dash_attr}/>"
    )
    .unwrap();
}

fn lookup_edge_endpoints<'a>(
    graph: &'a FlowGraph,
    edge: &FlowEdge,
) -> Option<(&'a FlowNode, &'a FlowNode)> {
    let from = graph.nodes.iter().find(|n| n.id == edge.from)?;
    let to = graph.nodes.iter().find(|n| n.id == edge.to)?;
    Some((from, to))
}

fn node_palette(kind: FlowNodeKind) -> (&'static str, &'static str) {
    match kind {
        FlowNodeKind::Inference => ("#e3f2fd", "#1565c0"),
        FlowNodeKind::ToolUse => ("#fff3e0", "#ef6c00"),
        FlowNodeKind::Subagent => ("#f3e5f5", "#6a1b9a"),
        FlowNodeKind::Skill => ("#e8f5e9", "#2e7d32"),
    }
}

fn edge_palette(kind: FlowEdgeKind) -> (&'static str, &'static str) {
    match kind {
        FlowEdgeKind::Default => ("#9e9e9e", ""),
        FlowEdgeKind::Dispatch => ("#ef6c00", "4 3"),
        FlowEdgeKind::Return => ("#ef6c00", "4 3"),
        FlowEdgeKind::Subagent => ("#1565c0", ""),
        FlowEdgeKind::Unattached => ("#b71c1c", "2 2"),
    }
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn svg_truncate(label: &str, max_chars: usize) -> String {
    let count = label.chars().count();
    if count <= max_chars {
        return label.to_string();
    }
    let mut out: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// Suppress unused-import warning if INTER_TURN_GAP / RAIL_GAP get
// retired from the renderer; they're imported here as a guard against
// drift between the SDK constants and the SVG layout assumptions, and
// referenced explicitly in this static assertion.
const _ASSERT_LAYOUT_CONSTANTS: () = {
    // NODE_HEIGHT must be < RAIL_GAP so the connector line between
    // stacked nodes has non-zero length. The renderer relies on this
    // invariant to draw the vertical sequencing edges.
    assert!(NODE_HEIGHT < RAIL_GAP);
    // NODE_WIDTH must be smaller than INTER_TURN_GAP for adjacent
    // turn columns to leave breathing room between bars.
    assert!(NODE_WIDTH < INTER_TURN_GAP);
};

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_sdk::{
        flow_graph_from_trees, FlowOpts as SdkFlowOpts, SpanAttrValue, SpanKind, SpanNode,
        TurnSpanTree,
    };

    fn turn_root() -> SpanNode {
        SpanNode::new(SpanKind::Turn, "turn")
    }

    fn inf(model: &str) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::Inference, model);
        n.set_attr("model", SpanAttrValue::str(model));
        n.set_attr("tokens.input", SpanAttrValue::Int(100));
        n
    }

    fn tu(name: &str, id: &str) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::ToolUse, name);
        n.set_attr("tool_use_id", SpanAttrValue::str(id));
        n
    }

    fn tree(turn_number: u32, root: SpanNode) -> TurnSpanTree {
        TurnSpanTree {
            session_id: "sess-1".into(),
            turn_id: format!("msg-{turn_number}"),
            turn_number,
            root,
        }
    }

    #[test]
    fn mermaid_renders_a_minimal_graph() {
        let mut root = turn_root();
        let mut i = inf("claude");
        i.children.push(tu("Bash", "tu-a"));
        root.children.push(i);
        let g = flow_graph_from_trees("sess-1", &[tree(0, root)], SdkFlowOpts::default());
        let out = render_mermaid(&g);
        assert!(out.starts_with("```mermaid\n"));
        assert!(out.ends_with("```\n"));
        assert!(out.contains("graph LR"));
        assert!(out.contains("classDef inference"));
        // Default sequential edge uses `-->`.
        assert!(out.contains(" --> "), "expected default arrow in: {out}");
    }

    #[test]
    fn mermaid_id_sanitizes_special_chars() {
        assert_eq!(mermaid_id("msg-1:inf-0"), "msg_1_inf_0");
        assert_eq!(mermaid_id("plain"), "plain");
    }

    #[test]
    fn mermaid_label_strips_quotes_and_brackets() {
        assert_eq!(mermaid_label("hello \"world\"", 3), "T3: hello world");
        assert_eq!(mermaid_label("a<b>c", 0), "T0: abc");
    }

    #[test]
    fn svg_starts_with_xml_declaration_and_self_contains() {
        let mut root = turn_root();
        root.children.push(inf("claude"));
        let g = flow_graph_from_trees("sess-1", &[tree(0, root)], SdkFlowOpts::default());
        let out = render_svg(&g);
        assert!(out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(out.contains("<svg"));
        assert!(out.contains("</svg>"));
        // No script tag — the static SVG must be safe to embed anywhere.
        assert!(!out.contains("<script"));
        // Legend is always present.
        assert!(out.contains("id=\"legend\""));
    }

    #[test]
    fn svg_truncated_label_uses_ellipsis() {
        assert_eq!(svg_truncate("short", 12), "short");
        assert_eq!(svg_truncate("0123456789abcdef", 12), "0123456789a…");
    }

    #[test]
    fn escape_xml_handles_all_five_predefined_entities() {
        assert_eq!(escape_xml("<&>\""), "&lt;&amp;&gt;&quot;");
        assert_eq!(escape_xml("a'b"), "a&apos;b");
    }

    #[test]
    fn svg_width_honors_legend_minimum_on_small_graphs() {
        // Tiny graph (1 turn, 1 inference) — `max_x + SVG_MARGIN * 2`
        // alone would be far below LEGEND_MIN_WIDTH, so we expect the
        // legend-min branch to dominate.
        let mut root = turn_root();
        root.children.push(inf("claude"));
        let g = flow_graph_from_trees("sess-1", &[tree(0, root)], SdkFlowOpts::default());
        let out = render_svg(&g);
        // Extract the width attribute from the `<svg>` open tag.
        let after = out.split("width=\"").nth(1).expect("svg width attr");
        let width: i32 = after
            .split('"')
            .next()
            .unwrap()
            .parse()
            .expect("width is integer");
        assert!(
            width >= LEGEND_MIN_WIDTH,
            "small-graph SVG width ({width}) must respect LEGEND_MIN_WIDTH ({LEGEND_MIN_WIDTH})"
        );
    }

    #[test]
    fn svg_width_exceeds_legend_minimum_on_wide_graphs() {
        // Wide graph: 10 turns → max_x = 10 * INTER_TURN_GAP + NODE_WIDTH
        // = 1048, which is well past LEGEND_MIN_WIDTH (~530). The
        // graph-bounds branch should win and the resulting width should
        // include the right SVG_MARGIN.
        let trees: Vec<TurnSpanTree> = (0..10)
            .map(|i| {
                let mut root = turn_root();
                root.children.push(inf("claude"));
                tree(i, root)
            })
            .collect();
        let g = flow_graph_from_trees("sess-1", &trees, SdkFlowOpts::default());
        let out = render_svg(&g);
        let after = out.split("width=\"").nth(1).expect("svg width attr");
        let width: i32 = after
            .split('"')
            .next()
            .unwrap()
            .parse()
            .expect("width is integer");
        assert!(
            width > LEGEND_MIN_WIDTH,
            "wide-graph SVG width ({width}) should exceed LEGEND_MIN_WIDTH ({LEGEND_MIN_WIDTH}) — the node-bounds branch should dominate"
        );
    }

    #[test]
    fn mermaid_dispatch_edge_carries_label() {
        let mut task = tu("Task", "tu-task");
        let mut sub = SpanNode::new(SpanKind::Subagent, "reviewer");
        sub.set_attr("agent_id", SpanAttrValue::str("agent-1"));
        task.children.push(sub);
        let mut i = inf("claude");
        i.children.push(task);
        let mut root = turn_root();
        root.children.push(i);
        let g = flow_graph_from_trees("sess-1", &[tree(0, root)], SdkFlowOpts::default());
        let out = render_mermaid(&g);
        assert!(out.contains("|dispatch|"), "got {out}");
    }
}
