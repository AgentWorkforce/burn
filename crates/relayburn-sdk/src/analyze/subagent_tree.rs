//! Subagent tree / per-type rollups — Rust port of
//! `packages/analyze/src/subagent-tree.ts`.
//!
//! Builds one tree per session from `SessionRelationshipRecord` rows, with
//! cost rolled up from leaves. Per-turn `TurnRecord.subagent` fields are folded
//! in to attach turn cost and to fill gaps for sessions whose relationship rows
//! are sparse (so a ledger with only the always-emitted Root rows still
//! reconstructs its subagent sidechains). Callers that pass no relationship
//! rows get a tree built from `TurnRecord.subagent` alone.

use crate::reader::{RelationshipType, SessionRelationshipRecord, TurnRecord};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};

use crate::analyze::cost::cost_for_turn;
use crate::analyze::pricing::PricingTable;
use crate::session_metrics::turn_total_tokens;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTreeNode {
    pub node_id: String,
    pub label: String,
    pub relationship_type: RelationshipType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub models: Vec<String>,
    pub self_turns: u64,
    /// Billable tokens of this node's own turns.
    pub self_tokens: u64,
    /// USD of this node's own turns; `None` (unknown) once any is unpriced.
    pub self_cost: Option<f64>,
    pub cumulative_turns: u64,
    /// Billable tokens of this node and every node under it.
    pub cumulative_tokens: u64,
    /// USD of this node and every node under it; `None` (unknown) once any
    /// turn in the subtree is unpriced.
    pub cumulative_cost: Option<f64>,
    pub depth: i32,
    pub children: Vec<SubagentTreeNode>,
}

#[derive(Debug, Clone)]
pub(crate) struct BuildSubagentTreeOptions<'a> {
    pub pricing: &'a PricingTable,
    pub relationships: Option<&'a [SessionRelationshipRecord]>,
}

impl<'a> BuildSubagentTreeOptions<'a> {
    pub fn new(pricing: &'a PricingTable) -> Self {
        Self {
            pricing,
            relationships: None,
        }
    }

    pub fn with_relationships(mut self, rels: &'a [SessionRelationshipRecord]) -> Self {
        self.relationships = Some(rels);
        self
    }
}

/// Build per-session subagent trees. Each session yields one tree whose root
/// is the main thread. Children are subagent invocations (grouped by
/// `subagent.agentId`), nested by `parentAgentId`. When relationship rows are
/// supplied, they are the primary substrate; per-turn `subagent` fields
/// attach turn cost and fill legacy gaps.
pub(crate) fn build_subagent_tree(
    turns: &[TurnRecord],
    opts: &BuildSubagentTreeOptions<'_>,
) -> IndexMap<String, SubagentTreeNode> {
    let relationships = opts.relationships.unwrap_or(&[]);
    build_relationship_trees(turns, relationships, opts.pricing)
}

#[derive(Debug)]
struct MutableNode {
    node_id: String,
    label: String,
    relationship_type: RelationshipType,
    subagent_type: Option<String>,
    description: Option<String>,
    self_spend: Spend,
    cumulative_spend: Spend,
    depth: i32,
    children: Vec<String>,
}

/// Turns, tokens and USD of a set of turns; USD is unknown once any turn
/// is unpriced.
#[derive(Debug, Default, Clone, Copy)]
struct Spend {
    turns: u64,
    tokens: u64,
    usd: f64,
    unpriced: bool,
}

impl Spend {
    fn add(&mut self, other: &Spend) {
        self.turns += other.turns;
        self.tokens += other.tokens;
        self.usd += other.usd;
        self.unpriced |= other.unpriced;
    }

    fn cost(&self) -> Option<f64> {
        (!self.unpriced).then_some(self.usd)
    }
}

impl MutableNode {
    fn new(id: String, label: String, relationship_type: RelationshipType) -> Self {
        Self {
            node_id: id,
            label,
            relationship_type,
            subagent_type: None,
            description: None,
            self_spend: Spend::default(),
            cumulative_spend: Spend::default(),
            depth: -1,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
struct GraphState {
    alias_by_id: IndexMap<String, String>,
    node_by_id: IndexMap<String, MutableNode>,
    models_by_node: IndexMap<String, IndexSet<String>>,
    parent_by_node: IndexMap<String, String>,
}

fn build_relationship_trees(
    turns: &[TurnRecord],
    relationships: &[SessionRelationshipRecord],
    pricing: &PricingTable,
) -> IndexMap<String, SubagentTreeNode> {
    let mut state = GraphState {
        alias_by_id: build_relationship_aliases(turns, relationships),
        ..GraphState::default()
    };

    for r in relationships {
        let id = canonical_id(&state, &relationship_node_id(r));
        ensure_node(
            &mut state,
            &id,
            &label_for_relationship(r),
            r.relationship_type,
        );
        apply_relationship_metadata(&mut state, &id, r);
        if r.relationship_type == RelationshipType::Root {
            continue;
        }
        let Some(related) = &r.related_session_id else {
            continue;
        };
        let parent_id = canonical_id(&state, related);
        ensure_node(&mut state, &parent_id, &parent_id, RelationshipType::Root);
        if !state.parent_by_node.contains_key(&id) {
            state.parent_by_node.insert(id.clone(), parent_id);
        }
    }

    add_legacy_subagent_gaps(&mut state, turns);
    ensure_turn_session_roots(&mut state, turns);
    attach_graph_children(&mut state);
    attach_turn_costs(&mut state, turns, pricing);

    let child_ids = collect_attached_child_ids(&state);
    let root_ids: Vec<String> = state
        .node_by_id
        .keys()
        .filter(|id| !child_ids.contains(*id))
        .cloned()
        .collect();

    let mut out: IndexMap<String, SubagentTreeNode> = IndexMap::new();
    for id in root_ids {
        finalize_tree(&mut state, &id);
        let tree = materialize_session_tree(&state.node_by_id, &state.models_by_node, &id);
        out.insert(id, tree);
    }
    out
}

fn build_relationship_aliases(
    turns: &[TurnRecord],
    relationships: &[SessionRelationshipRecord],
) -> IndexMap<String, String> {
    let mut sessions_with_native_sidechains: IndexSet<String> = IndexSet::new();
    for t in turns {
        if let Some(sub) = &t.subagent {
            if sub.agent_id.is_some() {
                sessions_with_native_sidechains.insert(t.session_id.clone());
            }
        }
    }
    for r in relationships {
        if r.relationship_type == RelationshipType::Subagent {
            if let Some(rs) = &r.related_session_id {
                if rs == &r.session_id {
                    sessions_with_native_sidechains.insert(r.session_id.clone());
                }
            }
        }
    }

    let mut aliases: IndexMap<String, String> = IndexMap::new();
    for r in relationships {
        aliases.insert(r.session_id.clone(), r.session_id.clone());
    }
    for r in relationships {
        if r.relationship_type != RelationshipType::Subagent {
            continue;
        }
        let Some(agent_id) = &r.agent_id else {
            aliases.insert(r.session_id.clone(), r.session_id.clone());
            continue;
        };
        let target = if sessions_with_native_sidechains.contains(&r.session_id) {
            agent_id.clone()
        } else {
            r.session_id.clone()
        };
        aliases.insert(agent_id.clone(), target);
    }
    aliases
}

fn relationship_node_id(r: &SessionRelationshipRecord) -> String {
    if r.relationship_type == RelationshipType::Subagent {
        r.agent_id.clone().unwrap_or_else(|| r.session_id.clone())
    } else {
        r.session_id.clone()
    }
}

fn canonical_id(state: &GraphState, id: &str) -> String {
    state
        .alias_by_id
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

fn ensure_node(state: &mut GraphState, id: &str, label: &str, relationship_type: RelationshipType) {
    if !state.node_by_id.contains_key(id) {
        state.node_by_id.insert(
            id.to_string(),
            MutableNode::new(id.to_string(), label.to_string(), relationship_type),
        );
        state.models_by_node.insert(id.to_string(), IndexSet::new());
    }
}

fn label_for_relationship(r: &SessionRelationshipRecord) -> String {
    match r.relationship_type {
        RelationshipType::Root => "main".to_string(),
        RelationshipType::Subagent => r
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string()),
        _ => r.session_id.clone(),
    }
}

fn apply_relationship_metadata(state: &mut GraphState, id: &str, r: &SessionRelationshipRecord) {
    let node = state.node_by_id.get_mut(id).unwrap();
    if r.relationship_type == RelationshipType::Root {
        if node.relationship_type == RelationshipType::Root {
            node.label = "main".to_string();
        }
        return;
    }
    node.relationship_type = r.relationship_type;
    node.label = label_for_relationship(r);
    if let Some(st) = &r.subagent_type {
        node.subagent_type = Some(st.clone());
    }
    if let Some(d) = &r.description {
        node.description = Some(d.clone());
    }
}

fn add_legacy_subagent_gaps(state: &mut GraphState, turns: &[TurnRecord]) {
    for t in turns {
        let Some(sub) = &t.subagent else { continue };
        let Some(agent_id) = &sub.agent_id else {
            continue;
        };
        let id = canonical_id(state, agent_id);
        let label = sub
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        ensure_node(state, &id, &label, RelationshipType::Subagent);
        let node = state.node_by_id.get_mut(&id).unwrap();
        if node.relationship_type == RelationshipType::Root {
            node.relationship_type = RelationshipType::Subagent;
        }
        if node.label == "(unknown)" {
            if let Some(st) = &sub.subagent_type {
                node.label = st.clone();
            }
        }
        if node.subagent_type.is_none() {
            if let Some(st) = &sub.subagent_type {
                node.subagent_type = Some(st.clone());
            }
        }
        if node.description.is_none() {
            if let Some(d) = &sub.description {
                node.description = Some(d.clone());
            }
        }
        if state.parent_by_node.contains_key(&id) {
            continue;
        }
        let parent_raw = sub
            .parent_agent_id
            .clone()
            .unwrap_or_else(|| t.session_id.clone());
        let parent_id = canonical_id(state, &parent_raw);
        state.parent_by_node.insert(id, parent_id);
    }
}

fn ensure_turn_session_roots(state: &mut GraphState, turns: &[TurnRecord]) {
    for t in turns {
        let id = canonical_id(state, &t.session_id);
        ensure_node(state, &id, "main", RelationshipType::Root);
        let node = state.node_by_id.get_mut(&id).unwrap();
        if node.relationship_type == RelationshipType::Root {
            node.label = "main".to_string();
        }
    }
    let parent_ids: Vec<String> = state.parent_by_node.values().cloned().collect();
    for pid in parent_ids {
        ensure_node(state, &pid, &pid, RelationshipType::Root);
    }
}

fn attach_graph_children(state: &mut GraphState) {
    let parent_map = state.parent_by_node.clone();
    for (id, parent_id) in parent_map.iter() {
        if !state.node_by_id.contains_key(id) {
            continue;
        }
        let Some(resolved) = resolve_graph_parent(id, parent_id, &parent_map) else {
            continue;
        };
        let Some(parent) = state.node_by_id.get_mut(&resolved) else {
            continue;
        };
        if !parent.children.contains(id) {
            parent.children.push(id.clone());
        }
    }
}

fn collect_attached_child_ids(state: &GraphState) -> IndexSet<String> {
    let mut out = IndexSet::new();
    for node in state.node_by_id.values() {
        for c in &node.children {
            out.insert(c.clone());
        }
    }
    out
}

fn attach_turn_costs(state: &mut GraphState, turns: &[TurnRecord], pricing: &PricingTable) {
    let mut unresolved_by_parent: IndexMap<String, String> = IndexMap::new();
    for t in turns {
        let cost = cost_for_turn(t, pricing).map(|c| c.total);
        let sub = t.subagent.as_ref();
        if let Some(s) = sub {
            if s.agent_id.is_none() {
                let parent_id = canonical_id(state, &t.session_id);
                let unresolved_id = if let Some(existing) = unresolved_by_parent.get(&parent_id) {
                    existing.clone()
                } else {
                    let uid = format!("{parent_id}:__unresolved");
                    ensure_node(state, &uid, "(unresolved)", RelationshipType::Subagent);
                    state.parent_by_node.insert(uid.clone(), parent_id.clone());
                    if let Some(parent) = state.node_by_id.get_mut(&parent_id) {
                        if !parent.children.contains(&uid) {
                            parent.children.push(uid.clone());
                        }
                    }
                    unresolved_by_parent.insert(parent_id.clone(), uid.clone());
                    uid
                };
                add_turn_to_node(state, &unresolved_id, t, cost);
                continue;
            }
        }
        let id = match sub.and_then(|s| s.agent_id.as_deref()) {
            Some(a) => canonical_id(state, a),
            None => canonical_id(state, &t.session_id),
        };
        let label = sub
            .and_then(|s| s.subagent_type.clone())
            .unwrap_or_else(|| "main".to_string());
        let rel = if sub.is_some() {
            RelationshipType::Subagent
        } else {
            RelationshipType::Root
        };
        ensure_node(state, &id, &label, rel);
        add_turn_to_node(state, &id, t, cost);
    }
}

fn add_turn_to_node(state: &mut GraphState, id: &str, turn: &TurnRecord, cost: Option<f64>) {
    let Some(node) = state.node_by_id.get_mut(id) else {
        return;
    };
    node.self_spend.add(&Spend {
        turns: 1,
        tokens: turn_total_tokens(turn),
        usd: cost.unwrap_or(0.0),
        unpriced: cost.is_none(),
    });
    if !turn.model.is_empty() {
        let entry = state.models_by_node.entry(id.to_string()).or_default();
        entry.insert(turn.model.clone());
    }
}

fn finalize_tree(state: &mut GraphState, root_id: &str) {
    // BFS depth assignment with cycle protection.
    let mut queue: std::collections::VecDeque<(String, i32)> = std::collections::VecDeque::new();
    queue.push_back((root_id.to_string(), 0));
    let mut seen: IndexSet<String> = IndexSet::new();
    while let Some((id, depth)) = queue.pop_front() {
        if seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());
        let children = if let Some(n) = state.node_by_id.get_mut(&id) {
            n.depth = depth;
            n.children.clone()
        } else {
            continue;
        };
        for c in children {
            queue.push_back((c, depth + 1));
        }
    }

    fold_cumulative(&mut state.node_by_id, root_id);
    sort_tree(&mut state.node_by_id, root_id);
}

fn fold_cumulative(nodes: &mut IndexMap<String, MutableNode>, root_id: &str) {
    let order = topo_post_order(nodes, root_id);
    for id in order {
        let (mut spend, children) = {
            let n = nodes.get(&id).unwrap();
            (n.self_spend, n.children.clone())
        };
        for c in &children {
            if let Some(child) = nodes.get(c) {
                spend.add(&child.cumulative_spend);
            }
        }
        nodes.get_mut(&id).unwrap().cumulative_spend = spend;
    }
}

fn topo_post_order(nodes: &IndexMap<String, MutableNode>, root_id: &str) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: IndexSet<String> = IndexSet::new();
    fn visit(
        nodes: &IndexMap<String, MutableNode>,
        id: &str,
        seen: &mut IndexSet<String>,
        order: &mut Vec<String>,
    ) {
        if seen.contains(id) {
            return;
        }
        seen.insert(id.to_string());
        if let Some(n) = nodes.get(id) {
            for c in n.children.clone() {
                visit(nodes, &c, seen, order);
            }
        }
        order.push(id.to_string());
    }
    visit(nodes, root_id, &mut seen, &mut order);
    order
}

fn sort_tree(nodes: &mut IndexMap<String, MutableNode>, root_id: &str) {
    let order = topo_post_order(nodes, root_id);
    for id in order {
        let mut children = nodes.get(&id).unwrap().children.clone();
        let spend = |id: &String| {
            nodes
                .get(id)
                .map(|n| n.cumulative_spend)
                .unwrap_or_default()
        };
        children.sort_by(|a, b| spend_order(&spend(b), &spend(a)));
        nodes.get_mut(&id).unwrap().children = children;
    }
}

/// Larger spend first: by USD when both are priced, else by tokens.
fn spend_order(a: &Spend, b: &Spend) -> std::cmp::Ordering {
    match (a.cost(), b.cost()) {
        (Some(ca), Some(cb)) => ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.tokens.cmp(&b.tokens),
    }
}

fn resolve_graph_parent(
    id: &str,
    parent_id: &str,
    parent_by_node: &IndexMap<String, String>,
) -> Option<String> {
    if parent_id == id {
        return None;
    }
    let mut seen: IndexSet<String> = IndexSet::new();
    seen.insert(id.to_string());
    let mut cursor = parent_id.to_string();
    while parent_by_node.contains_key(&cursor) {
        if seen.contains(&cursor) {
            return None;
        }
        seen.insert(cursor.clone());
        cursor = parent_by_node.get(&cursor).unwrap().clone();
    }
    Some(parent_id.to_string())
}

fn materialize_session_tree(
    nodes: &IndexMap<String, MutableNode>,
    models: &IndexMap<String, IndexSet<String>>,
    root_id: &str,
) -> SubagentTreeNode {
    let n = nodes.get(root_id).unwrap();
    let mut model_vec: Vec<String> = models
        .get(root_id)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    model_vec.sort();
    let mut children = Vec::with_capacity(n.children.len());
    for c in &n.children {
        children.push(materialize_session_tree(nodes, models, c));
    }
    SubagentTreeNode {
        node_id: n.node_id.clone(),
        label: n.label.clone(),
        relationship_type: n.relationship_type,
        subagent_type: n.subagent_type.clone(),
        description: n.description.clone(),
        models: model_vec,
        self_turns: n.self_spend.turns,
        self_tokens: n.self_spend.tokens,
        self_cost: n.self_spend.cost(),
        cumulative_turns: n.cumulative_spend.turns,
        cumulative_tokens: n.cumulative_spend.tokens,
        cumulative_cost: n.cumulative_spend.cost(),
        depth: n.depth,
        children,
    }
}

#[cfg(test)]
#[path = "subagent_tree_tests.rs"]
mod tests;
