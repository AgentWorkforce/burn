//! The `subagents` block of `burn analyze`: the session's subagent tree,
//! each node with its own and cumulative turns, tokens and cost.

use relayburn_sdk::SubagentTreeNode;

use crate::render::format::{format_cost, format_uint};

/// One summary line, then the tree when the session spawned subagents.
pub(super) fn subagent_lines(root: &SubagentTreeNode) -> (String, Vec<String>) {
    let count = descendants(root);
    if count == 0 {
        return ("none".to_string(), Vec::new());
    }
    let own = root.self_tokens;
    let delegated = root.cumulative_tokens.saturating_sub(own);
    let summary = format!(
        "{count} · {} of {} tokens ({}) in subagents · total {}",
        format_uint(delegated),
        format_uint(root.cumulative_tokens),
        share(delegated, root.cumulative_tokens),
        format_cost(root.cumulative_cost),
    );
    let mut tree = vec![format!("  {}", node_line(root))];
    children(root, "  ", &mut tree);
    (summary, tree)
}

fn descendants(node: &SubagentTreeNode) -> usize {
    node.children.iter().map(|c| 1 + descendants(c)).sum()
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".to_string();
    }
    format!("{:.0}%", part as f64 * 100.0 / whole as f64)
}

fn children(node: &SubagentTreeNode, prefix: &str, out: &mut Vec<String>) {
    let last = node.children.len().saturating_sub(1);
    for (i, child) in node.children.iter().enumerate() {
        let (branch, next) = if i == last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        out.push(format!("{prefix}{branch}{}", node_line(child)));
        children(child, &format!("{prefix}{next}"), out);
    }
}

/// `label "description" (models)  self … · total …`; the total appears
/// only for a node with children.
fn node_line(node: &SubagentTreeNode) -> String {
    let description = node
        .description
        .as_deref()
        .map(|d| format!(" \"{d}\""))
        .unwrap_or_default();
    let models = if node.models.is_empty() {
        String::new()
    } else {
        format!(" ({})", node.models.join(", "))
    };
    let own = spend(node.self_turns, node.self_tokens, node.self_cost);
    let total = if node.children.is_empty() {
        String::new()
    } else {
        format!(
            " · total {}",
            spend(
                node.cumulative_turns,
                node.cumulative_tokens,
                node.cumulative_cost
            )
        )
    };
    format!("{}{description}{models}  {own}{total}", node.label)
}

fn spend(turns: u64, tokens: u64, cost: Option<f64>) -> String {
    format!(
        "{} turn{} · {} tokens · {}",
        format_uint(turns),
        if turns == 1 { "" } else { "s" },
        format_uint(tokens),
        format_cost(cost),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_sdk::RelationshipType;

    fn node(
        id: &str,
        tokens: u64,
        cost: Option<f64>,
        kids: Vec<SubagentTreeNode>,
    ) -> SubagentTreeNode {
        let cumulative_tokens = tokens + kids.iter().map(|k| k.cumulative_tokens).sum::<u64>();
        let cumulative_cost = cost.and_then(|c| {
            kids.iter()
                .try_fold(c, |sum, k| Some(sum + k.cumulative_cost?))
        });
        SubagentTreeNode {
            node_id: id.to_string(),
            label: id.to_string(),
            relationship_type: RelationshipType::Subagent,
            subagent_type: None,
            description: None,
            models: vec!["m".to_string()],
            self_turns: 1,
            self_tokens: tokens,
            self_cost: cost,
            cumulative_turns: 1 + kids.iter().map(|k| k.cumulative_turns).sum::<u64>(),
            cumulative_tokens,
            cumulative_cost,
            depth: 0,
            children: kids,
        }
    }

    #[test]
    fn a_session_without_subagents_renders_none() {
        let (summary, tree) = subagent_lines(&node("main", 10, Some(1.0), Vec::new()));
        assert_eq!(summary, "none");
        assert!(tree.is_empty());
    }

    #[test]
    fn nested_subagents_render_under_their_spawner() {
        let grandchild = node("reviewer", 25, Some(0.25), Vec::new());
        let child = node("explore", 50, None, vec![grandchild]);
        let root = node("main", 25, Some(0.5), vec![child]);
        let (summary, tree) = subagent_lines(&root);
        assert_eq!(
            summary,
            "2 · 75 of 100 tokens (75%) in subagents · total cost unknown"
        );
        assert_eq!(
            tree,
            vec![
                "  main (m)  1 turn · 25 tokens · $0.500 · total 3 turns · 100 tokens · cost unknown",
                "  └─ explore (m)  1 turn · 50 tokens · cost unknown · total 2 turns · 75 tokens · cost unknown",
                "     └─ reviewer (m)  1 turn · 25 tokens · $0.250",
            ]
        );
    }
}
