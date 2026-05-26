//! Subagent tree / per-type rollups — Rust port of
//! `packages/analyze/src/subagent-tree.ts`.
//!
//! Walks the parent-uuid chains in `TurnRecord.subagent` (or
//! `SessionRelationshipRecord` rows when supplied) to build one tree per
//! session, with cost rolled up from leaves. The relationship-row path is
//! the primary substrate for newer ingests; the legacy path falls back to
//! `TurnRecord.subagent` only.

use crate::reader::{RelationshipType, SessionRelationshipRecord, TurnRecord};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};

use crate::analyze::cost::cost_for_turn;
use crate::analyze::pricing::PricingTable;

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
    pub self_cost: f64,
    pub cumulative_turns: u64,
    pub cumulative_cost: f64,
    pub depth: i32,
    pub children: Vec<SubagentTreeNode>,
}

#[derive(Debug, Clone)]
pub struct BuildSubagentTreeOptions<'a> {
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
pub fn build_subagent_tree(
    turns: &[TurnRecord],
    opts: &BuildSubagentTreeOptions<'_>,
) -> IndexMap<String, SubagentTreeNode> {
    if let Some(rels) = opts.relationships {
        if !rels.is_empty() {
            return build_relationship_trees(turns, rels, opts.pricing);
        }
    }
    build_legacy_subagent_trees(turns, opts.pricing)
}

#[derive(Debug)]
struct MutableNode {
    node_id: String,
    label: String,
    relationship_type: RelationshipType,
    subagent_type: Option<String>,
    description: Option<String>,
    self_turns: u64,
    self_cost: f64,
    cumulative_turns: u64,
    cumulative_cost: f64,
    depth: i32,
    children: Vec<String>,
}

impl MutableNode {
    fn new(id: String, label: String, relationship_type: RelationshipType) -> Self {
        Self {
            node_id: id,
            label,
            relationship_type,
            subagent_type: None,
            description: None,
            self_turns: 0,
            self_cost: 0.0,
            cumulative_turns: 0,
            cumulative_cost: 0.0,
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

fn build_legacy_subagent_trees(
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> IndexMap<String, SubagentTreeNode> {
    let mut by_session: IndexMap<String, Vec<&TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session.entry(t.session_id.clone()).or_default().push(t);
    }
    let mut out: IndexMap<String, SubagentTreeNode> = IndexMap::new();
    for (session_id, session_turns) in by_session {
        let root = build_session_tree(&session_id, &session_turns, pricing);
        out.insert(session_id, root);
    }
    out
}

fn build_session_tree(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> SubagentTreeNode {
    let mut nodes: IndexMap<String, MutableNode> = IndexMap::new();
    let mut models: IndexMap<String, IndexSet<String>> = IndexMap::new();
    nodes.insert(
        session_id.to_string(),
        MutableNode {
            depth: 0,
            ..MutableNode::new(
                session_id.to_string(),
                "main".to_string(),
                RelationshipType::Root,
            )
        },
    );
    models.insert(session_id.to_string(), IndexSet::new());

    let unresolved_id = format!("{session_id}:__unresolved");
    let mut unresolved_created = false;

    for t in turns {
        let cost = cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0);
        let Some(sub) = &t.subagent else {
            let node = nodes.get_mut(session_id).unwrap();
            node.self_turns += 1;
            node.self_cost += cost;
            if !t.model.is_empty() {
                models.get_mut(session_id).unwrap().insert(t.model.clone());
            }
            continue;
        };
        let Some(agent_id) = &sub.agent_id else {
            if !unresolved_created {
                let mut un = MutableNode::new(
                    unresolved_id.clone(),
                    "(unresolved)".to_string(),
                    RelationshipType::Subagent,
                );
                un.depth = 1;
                nodes.insert(unresolved_id.clone(), un);
                models.insert(unresolved_id.clone(), IndexSet::new());
                nodes
                    .get_mut(session_id)
                    .unwrap()
                    .children
                    .push(unresolved_id.clone());
                unresolved_created = true;
            }
            let n = nodes.get_mut(&unresolved_id).unwrap();
            n.self_turns += 1;
            n.self_cost += cost;
            if !t.model.is_empty() {
                models
                    .get_mut(&unresolved_id)
                    .unwrap()
                    .insert(t.model.clone());
            }
            continue;
        };
        if !nodes.contains_key(agent_id) {
            let mut n = MutableNode::new(
                agent_id.clone(),
                sub.subagent_type
                    .clone()
                    .unwrap_or_else(|| "(unknown)".to_string()),
                RelationshipType::Subagent,
            );
            n.subagent_type = sub.subagent_type.clone();
            n.description = sub.description.clone();
            nodes.insert(agent_id.clone(), n);
            models.insert(agent_id.clone(), IndexSet::new());
        } else {
            let n = nodes.get_mut(agent_id).unwrap();
            if n.subagent_type.is_none() {
                if let Some(st) = &sub.subagent_type {
                    n.subagent_type = Some(st.clone());
                    if n.label == "(unknown)" {
                        n.label = st.clone();
                    }
                }
            }
            if n.description.is_none() {
                if let Some(d) = &sub.description {
                    n.description = Some(d.clone());
                }
            }
        }
        let n = nodes.get_mut(agent_id).unwrap();
        n.self_turns += 1;
        n.self_cost += cost;
        if !t.model.is_empty() {
            models.get_mut(agent_id).unwrap().insert(t.model.clone());
        }
    }

    // Build parent map (insertion order = first-encounter order in turns).
    let mut parent_by_node: IndexMap<String, String> = IndexMap::new();
    for t in turns {
        let Some(sub) = &t.subagent else { continue };
        let Some(agent_id) = &sub.agent_id else {
            continue;
        };
        if parent_by_node.contains_key(agent_id) {
            continue;
        }
        let pid = sub
            .parent_agent_id
            .clone()
            .unwrap_or_else(|| session_id.to_string());
        parent_by_node.insert(agent_id.clone(), pid);
    }

    // Attach children, redirecting cycles / self-parents to the session root.
    for (id, parent_id) in parent_by_node.clone() {
        if !nodes.contains_key(&id) {
            continue;
        }
        let resolved = resolve_parent_or_root(&id, &parent_id, &parent_by_node, session_id);
        let parent_target = if nodes.contains_key(&resolved) {
            resolved
        } else {
            session_id.to_string()
        };
        let parent_node = nodes.get_mut(&parent_target).unwrap();
        parent_node.children.push(id);
    }

    // BFS depth assignment.
    assign_depth(&mut nodes, session_id);

    fold_cumulative(&mut nodes, session_id);
    sort_tree(&mut nodes, session_id);

    materialize_session_tree(&nodes, &models, session_id)
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
        let cost = cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0);
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

fn add_turn_to_node(state: &mut GraphState, id: &str, turn: &TurnRecord, cost: f64) {
    let Some(node) = state.node_by_id.get_mut(id) else {
        return;
    };
    node.self_turns += 1;
    node.self_cost += cost;
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

fn assign_depth(nodes: &mut IndexMap<String, MutableNode>, root_id: &str) {
    let mut queue: std::collections::VecDeque<(String, i32)> = std::collections::VecDeque::new();
    queue.push_back((root_id.to_string(), 0));
    let mut seen: IndexSet<String> = IndexSet::new();
    while let Some((id, depth)) = queue.pop_front() {
        if seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());
        let children = if let Some(n) = nodes.get_mut(&id) {
            n.depth = depth;
            n.children.clone()
        } else {
            continue;
        };
        for c in children {
            queue.push_back((c, depth + 1));
        }
    }
}

fn fold_cumulative(nodes: &mut IndexMap<String, MutableNode>, root_id: &str) {
    let order = topo_post_order(nodes, root_id);
    for id in order {
        let (self_cost, self_turns, children) = {
            let n = nodes.get(&id).unwrap();
            (n.self_cost, n.self_turns, n.children.clone())
        };
        let mut cost = self_cost;
        let mut turns = self_turns;
        for c in &children {
            if let Some(child) = nodes.get(c) {
                cost += child.cumulative_cost;
                turns += child.cumulative_turns;
            }
        }
        let n = nodes.get_mut(&id).unwrap();
        n.cumulative_cost = cost;
        n.cumulative_turns = turns;
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
        children.sort_by(|a, b| {
            let ca = nodes.get(a).map(|n| n.cumulative_cost).unwrap_or(0.0);
            let cb = nodes.get(b).map(|n| n.cumulative_cost).unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        nodes.get_mut(&id).unwrap().children = children;
    }
}

fn resolve_parent_or_root(
    id: &str,
    parent_id: &str,
    parent_by_node: &IndexMap<String, String>,
    session_id: &str,
) -> String {
    if parent_id == id {
        return session_id.to_string();
    }
    let mut seen: IndexSet<String> = IndexSet::new();
    seen.insert(id.to_string());
    let mut cursor = parent_id.to_string();
    while cursor != session_id {
        if seen.contains(&cursor) {
            return session_id.to_string();
        }
        seen.insert(cursor.clone());
        match parent_by_node.get(&cursor) {
            Some(next) => cursor = next.clone(),
            None => return parent_id.to_string(),
        }
    }
    parent_id.to_string()
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
        self_turns: n.self_turns,
        self_cost: n.self_cost,
        cumulative_turns: n.cumulative_turns,
        cumulative_cost: n.cumulative_cost,
        depth: n.depth,
        children,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTypeStats {
    pub subagent_type: String,
    pub invocations: u64,
    pub turns: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

/// Aggregate subagent invocations across sessions by `subagentType`. An
/// invocation is the unique `(sessionId, agentId)` pair so the same agent id
/// re-used across sessions doesn't collide.
pub fn aggregate_subagent_type_stats(
    turns: &[TurnRecord],
    opts: &BuildSubagentTreeOptions<'_>,
) -> Vec<SubagentTypeStats> {
    #[derive(Default)]
    struct Inv {
        ty: String,
        turns: u64,
        cost: f64,
    }
    let mut by_invocation: IndexMap<String, Inv> = IndexMap::new();
    for t in turns {
        let Some(sub) = &t.subagent else { continue };
        let Some(agent_id) = &sub.agent_id else {
            continue;
        };
        let ty = sub
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        let key = format!("{}:{}", t.session_id, agent_id);
        let inv = by_invocation.entry(key).or_insert_with(|| Inv {
            ty: ty.clone(),
            turns: 0,
            cost: 0.0,
        });
        if inv.ty == "(unknown)" && ty != "(unknown)" {
            inv.ty = ty;
        }
        inv.turns += 1;
        inv.cost += cost_for_turn(t, opts.pricing)
            .map(|c| c.total)
            .unwrap_or(0.0);
    }
    let mut by_type: IndexMap<String, Vec<f64>> = IndexMap::new();
    let mut totals_by_type: IndexMap<String, (u64, f64)> = IndexMap::new();
    for inv in by_invocation.values() {
        by_type.entry(inv.ty.clone()).or_default().push(inv.cost);
        let entry = totals_by_type.entry(inv.ty.clone()).or_insert((0, 0.0));
        entry.0 += inv.turns;
        entry.1 += inv.cost;
    }
    let mut out: Vec<SubagentTypeStats> = Vec::new();
    for (ty, mut costs) in by_type {
        let (turns, total) = *totals_by_type.get(&ty).unwrap();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let invocations = costs.len() as u64;
        out.push(SubagentTypeStats {
            subagent_type: ty,
            invocations,
            turns,
            total_cost: total,
            median_cost: percentile(&costs, 0.5),
            p95_cost: percentile(&costs, 0.95),
            mean_cost: if invocations > 0 {
                total / invocations as f64
            } else {
                0.0
            },
        });
    }
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let len = sorted.len();
    // Nearest-rank with clamp.
    let raw = (p * len as f64).ceil() as i64 - 1;
    let rank = raw.clamp(0, len as i64 - 1) as usize;
    sorted[rank]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{
        RelationshipSourceKind, RelationshipType, SourceKind, Subagent, ToolCall, TurnRecord, Usage,
    };

    fn make_turn(
        session_id: &str,
        message_id: &str,
        model: &str,
        turn_index: u64,
        source: SourceKind,
        subagent: Option<Subagent>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index,
            ts: "2026-04-20T00:00:00.000Z".into(),
            model: model.into(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 1000,
                output: 1000,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: Vec::<ToolCall>::new(),
            files_touched: None,
            subagent,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn sub(
        agent_id: Option<&str>,
        parent_agent_id: Option<&str>,
        subagent_type: Option<&str>,
        description: Option<&str>,
    ) -> Subagent {
        Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: agent_id.map(String::from),
            parent_agent_id: parent_agent_id.map(String::from),
            subagent_type: subagent_type.map(String::from),
            description: description.map(String::from),
        }
    }

    fn rel(
        session_id: &str,
        rel_type: RelationshipType,
        related: Option<&str>,
        agent_id: Option<&str>,
        subagent_type: Option<&str>,
        description: Option<&str>,
        source: RelationshipSourceKind,
    ) -> SessionRelationshipRecord {
        SessionRelationshipRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            related_session_id: related.map(String::from),
            relationship_type: rel_type,
            ts: None,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: agent_id.map(String::from),
            subagent_type: subagent_type.map(String::from),
            description: description.map(String::from),
        }
    }

    #[test]
    fn folds_cumulative_cost_from_nested_subagents_up_to_the_main_root() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-1";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "m2",
                "claude-sonnet-4-6",
                1,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "o1",
                "claude-haiku-4-5",
                2,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    Some("Research"),
                )),
            ),
            make_turn(
                session_id,
                "o2",
                "claude-haiku-4-5",
                3,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    None,
                )),
            ),
            make_turn(
                session_id,
                "i1",
                "claude-haiku-4-5",
                4,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-inner"),
                    Some("u-outer"),
                    Some("code-reviewer"),
                    None,
                )),
            ),
        ];

        let opts = BuildSubagentTreeOptions::new(&pricing);
        let trees = build_subagent_tree(&turns, &opts);
        let root = trees.get(session_id).expect("root");
        assert_eq!(root.label, "main");
        assert_eq!(root.depth, 0);
        assert_eq!(root.self_turns, 2);
        assert_eq!(root.cumulative_turns, 5);
        assert!(root.cumulative_cost > root.self_cost);

        assert_eq!(root.children.len(), 1);
        let outer = &root.children[0];
        assert_eq!(outer.label, "Explore");
        assert_eq!(outer.depth, 1);
        assert_eq!(outer.self_turns, 2);
        assert_eq!(outer.cumulative_turns, 3);
        assert_eq!(outer.children.len(), 1);

        let inner = &outer.children[0];
        assert_eq!(inner.label, "code-reviewer");
        assert_eq!(inner.depth, 2);
        assert_eq!(inner.self_turns, 1);
        assert_eq!(inner.cumulative_turns, 1);
        assert!((inner.cumulative_cost - inner.self_cost).abs() < 1e-12);

        assert!(
            (outer.cumulative_cost - (outer.self_cost + inner.cumulative_cost)).abs() < 1e-12,
            "outer cumulative is selfCost + inner.cumulativeCost"
        );
    }

    #[test]
    fn buckets_sidechain_turns_without_agent_id_under_an_unresolved_node() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-2";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "s1",
                "claude-haiku-4-5",
                1,
                SourceKind::ClaudeCode,
                Some(Subagent {
                    is_sidechain: true,
                    parent_tool_use_id: None,
                    agent_id: None,
                    parent_agent_id: None,
                    subagent_type: None,
                    description: None,
                }),
            ),
        ];
        let opts = BuildSubagentTreeOptions::new(&pricing);
        let trees = build_subagent_tree(&turns, &opts);
        let root = trees.get(session_id).unwrap();
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].label, "(unresolved)");
        assert_eq!(root.children[0].self_turns, 1);
    }

    #[test]
    fn builds_the_same_claude_tree_from_session_relationship_records() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-graph";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "o1",
                "claude-haiku-4-5",
                1,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    Some("Research"),
                )),
            ),
            make_turn(
                session_id,
                "i1",
                "claude-haiku-4-5",
                2,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-inner"),
                    Some("u-outer"),
                    Some("code-reviewer"),
                    None,
                )),
            ),
        ];
        let relationships = vec![
            rel(
                session_id,
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::ClaudeCode,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some(session_id),
                Some("u-outer"),
                Some("Explore"),
                Some("Research"),
                RelationshipSourceKind::NativeClaude,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some("u-outer"),
                Some("u-inner"),
                Some("code-reviewer"),
                None,
                RelationshipSourceKind::NativeClaude,
            ),
        ];

        let legacy_opts = BuildSubagentTreeOptions::new(&pricing);
        let legacy = build_subagent_tree(&turns, &legacy_opts)
            .get(session_id)
            .unwrap()
            .clone();
        let graph_opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let graph = build_subagent_tree(&turns, &graph_opts)
            .get(session_id)
            .unwrap()
            .clone();
        assert_eq!(graph, legacy);
        assert_eq!(graph.relationship_type, RelationshipType::Root);
        assert_eq!(
            graph.children[0].relationship_type,
            RelationshipType::Subagent
        );
    }

    #[test]
    fn joins_child_session_relationship_rows_to_turns_without_per_turn_subagent_metadata() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            make_turn(
                "parent-session",
                "parent-1",
                "gpt-5.1-codex",
                0,
                SourceKind::Codex,
                None,
            ),
            make_turn(
                "child-session",
                "child-1",
                "gpt-5.1-codex",
                0,
                SourceKind::Codex,
                None,
            ),
        ];
        let relationships = vec![
            rel(
                "parent-session",
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::Codex,
            ),
            rel(
                "child-session",
                RelationshipType::Subagent,
                Some("parent-session"),
                Some("agent-child"),
                Some("worker"),
                None,
                RelationshipSourceKind::Codex,
            ),
        ];

        let opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let root = build_subagent_tree(&turns, &opts)
            .get("parent-session")
            .unwrap()
            .clone();
        assert_eq!(root.self_turns, 1);
        assert_eq!(root.cumulative_turns, 2);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].label, "worker");
        assert_eq!(root.children[0].node_id, "child-session");
        assert_eq!(
            root.children[0].relationship_type,
            RelationshipType::Subagent
        );
        assert_eq!(root.children[0].self_turns, 1);
    }

    #[test]
    fn does_not_alias_native_sidechain_session_roots_onto_agent_ids_when_turns_lack_subagent_fields(
    ) {
        let pricing = load_builtin_pricing();
        let session_id = "partial-claude";
        let turns = vec![make_turn(
            session_id,
            "main-1",
            "claude-sonnet-4-6",
            0,
            SourceKind::ClaudeCode,
            None,
        )];
        let relationships = vec![
            rel(
                session_id,
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::ClaudeCode,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some(session_id),
                Some("u-outer"),
                Some("Explore"),
                None,
                RelationshipSourceKind::NativeClaude,
            ),
        ];
        let opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let root = build_subagent_tree(&turns, &opts)
            .get(session_id)
            .unwrap()
            .clone();
        assert_eq!(root.node_id, session_id);
        assert_eq!(root.label, "main");
        assert_eq!(root.self_turns, 1);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].node_id, "u-outer");
        assert_eq!(root.children[0].self_turns, 0);
    }

    #[test]
    fn reports_median_p95_mean_total_per_subagent_type_across_invocations() {
        let pricing = load_builtin_pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            let agent_id = format!("u-exp-{i}");
            for j in 0..=i {
                turns.push(make_turn(
                    &format!("sess-{i}"),
                    &format!("m-{i}-{j}"),
                    "claude-haiku-4-5",
                    j as u64,
                    SourceKind::ClaudeCode,
                    Some(sub(Some(&agent_id), None, Some("Explore"), None)),
                ));
            }
        }
        turns.push(make_turn(
            "sess-rev",
            "mr",
            "claude-haiku-4-5",
            0,
            SourceKind::ClaudeCode,
            Some(sub(Some("u-rev"), None, Some("code-reviewer"), None)),
        ));

        let opts = BuildSubagentTreeOptions::new(&pricing);
        let stats = aggregate_subagent_type_stats(&turns, &opts);
        let explore = stats.iter().find(|s| s.subagent_type == "Explore").unwrap();
        assert_eq!(explore.invocations, 3);
        assert_eq!(explore.turns, 6);
        assert!(explore.median_cost > 0.0);
        assert!(explore.p95_cost >= explore.median_cost);
        assert!((explore.mean_cost - explore.total_cost / 3.0).abs() < 1e-12);

        let rev = stats
            .iter()
            .find(|s| s.subagent_type == "code-reviewer")
            .unwrap();
        assert_eq!(rev.invocations, 1);
        assert_eq!(rev.turns, 1);
        assert!((rev.median_cost - rev.total_cost).abs() < 1e-12);
        assert!((rev.p95_cost - rev.total_cost).abs() < 1e-12);
    }
}
