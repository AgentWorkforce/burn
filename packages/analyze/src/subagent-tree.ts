import type {
  RelationshipType,
  SessionRelationshipRecord,
  TurnRecord,
} from '@relayburn/reader';

import { costForTurn } from './cost.js';
import type { PricingTable } from './pricing.js';

export interface SubagentTreeNode {
  // Stable id: the session id for the main-thread root, or the subagent's
  // agentId (root user uuid) for subagent invocations.
  nodeId: string;
  // Human-readable label: 'main', or '<subagentType>'. Falls back to
  // '(unknown)' for sidechain turns whose tree fields couldn't be resolved
  // from passive data.
  label: string;
  // Relationship edge that introduced this node. The main-thread root is
  // `root`; legacy TurnRecord.subagent fallback nodes are `subagent`.
  relationshipType: RelationshipType;
  // Agent/Task subagent_type from the spawning tool input, when known.
  subagentType?: string;
  // Agent/Task description from the spawning tool input, when known.
  description?: string;
  // Distinct models used by turns at this node (not including descendants).
  // Most invocations use a single model; we surface the set for visibility.
  models: string[];
  // Direct turn count and cost at this node only.
  selfTurns: number;
  selfCost: number;
  // Rolled-up turn count and cost including all descendants.
  cumulativeTurns: number;
  cumulativeCost: number;
  // 0 for the main-thread root, 1 for first-level subagent, etc.
  depth: number;
  children: SubagentTreeNode[];
}

export interface BuildSubagentTreeOptions {
  pricing: PricingTable;
  relationships?: readonly SessionRelationshipRecord[];
}

// Build per-session subagent trees. Each session produces one tree whose root
// represents the main thread (non-sidechain turns). Children are subagent
// invocations grouped by `subagent.agentId`, nested by `parentAgentId`.
// When SessionRelationshipRecord rows are provided, they are the primary tree
// substrate and TurnRecord.subagent is used only to attach turn costs and fill
// legacy gaps.
//
// Sessions with sidechain turns whose tree fields are absent (e.g. incomplete
// incremental ingest) still emit a tree; those turns attach to a synthetic
// '(unresolved)' node under the main root so their cost isn't dropped.
export function buildSubagentTree(
  turns: TurnRecord[],
  opts: BuildSubagentTreeOptions,
): Map<string, SubagentTreeNode> {
  if (opts.relationships && opts.relationships.length > 0) {
    return buildRelationshipTrees(turns, opts.relationships, opts.pricing);
  }
  return buildLegacySubagentTrees(turns, opts.pricing);
}

function buildLegacySubagentTrees(
  turns: TurnRecord[],
  pricing: PricingTable,
): Map<string, SubagentTreeNode> {
  const bySession = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }

  const out = new Map<string, SubagentTreeNode>();
  for (const [sessionId, sessionTurns] of bySession) {
    const root = buildSessionTree(sessionId, sessionTurns, pricing);
    out.set(sessionId, root);
  }
  return out;
}

interface MutableNode extends SubagentTreeNode {
  children: MutableNode[];
}

interface GraphState {
  aliasById: Map<string, string>;
  nodeById: Map<string, MutableNode>;
  modelsByNode: Map<string, Set<string>>;
  parentByNode: Map<string, string>;
}

function buildRelationshipTrees(
  turns: TurnRecord[],
  relationships: readonly SessionRelationshipRecord[],
  pricing: PricingTable,
): Map<string, SubagentTreeNode> {
  const state: GraphState = {
    aliasById: buildRelationshipAliases(turns, relationships),
    nodeById: new Map(),
    modelsByNode: new Map(),
    parentByNode: new Map(),
  };

  for (const r of relationships) {
    const id = canonicalId(state, relationshipNodeId(r));
    const node = ensureNode(state, id, labelForRelationship(r), r.relationshipType);
    applyRelationshipMetadata(node, r);
    if (r.relationshipType === 'root' || r.relatedSessionId === undefined) continue;
    const parentId = canonicalId(state, r.relatedSessionId);
    ensureNode(state, parentId, parentId, 'root');
    if (!state.parentByNode.has(id)) state.parentByNode.set(id, parentId);
  }

  addLegacySubagentGaps(state, turns);
  ensureTurnSessionRoots(state, turns);
  attachGraphChildren(state);
  attachTurnCosts(state, turns, pricing);

  const out = new Map<string, SubagentTreeNode>();
  const childIds = collectAttachedChildIds(state);
  for (const [id, node] of state.nodeById) {
    if (childIds.has(id)) continue;
    finalizeTree(state, node);
    out.set(id, node);
  }
  return out;
}

function buildRelationshipAliases(
  turns: readonly TurnRecord[],
  relationships: readonly SessionRelationshipRecord[],
): Map<string, string> {
  const sessionsWithNativeSidechains = new Set<string>();
  for (const t of turns) {
    if (t.subagent?.agentId) sessionsWithNativeSidechains.add(t.sessionId);
  }
  for (const r of relationships) {
    if (
      r.relationshipType === 'subagent' &&
      r.relatedSessionId === r.sessionId
    ) {
      sessionsWithNativeSidechains.add(r.sessionId);
    }
  }

  const aliases = new Map<string, string>();
  for (const r of relationships) {
    aliases.set(r.sessionId, r.sessionId);
  }
  for (const r of relationships) {
    if (r.relationshipType !== 'subagent') continue;
    if (r.agentId === undefined) {
      aliases.set(r.sessionId, r.sessionId);
      continue;
    }
    // Claude sidechains live inside the parent file session, so their
    // relationship row has sessionId=<root session>, agentId=<sidechain id>,
    // and turns already carry subagent.agentId. Child-session sources such as
    // Codex/OpenCode keep turns under the child session id, so the agent id
    // aliases to the session id while the session id remains addressable.
    aliases.set(
      r.agentId,
      sessionsWithNativeSidechains.has(r.sessionId) ? r.agentId : r.sessionId,
    );
  }
  return aliases;
}

function relationshipNodeId(r: SessionRelationshipRecord): string {
  if (r.relationshipType === 'subagent') return r.agentId ?? r.sessionId;
  return r.sessionId;
}

function canonicalId(state: GraphState, id: string): string {
  return state.aliasById.get(id) ?? id;
}

function ensureNode(
  state: GraphState,
  id: string,
  label: string,
  relationshipType: RelationshipType,
): MutableNode {
  let node = state.nodeById.get(id);
  if (!node) {
    node = {
      nodeId: id,
      label,
      relationshipType,
      models: [],
      selfTurns: 0,
      selfCost: 0,
      cumulativeTurns: 0,
      cumulativeCost: 0,
      depth: -1,
      children: [],
    };
    state.nodeById.set(id, node);
    state.modelsByNode.set(id, new Set());
  }
  return node;
}

function labelForRelationship(r: SessionRelationshipRecord): string {
  if (r.relationshipType === 'root') return 'main';
  if (r.relationshipType === 'subagent') return r.subagentType ?? '(unknown)';
  return r.sessionId;
}

function applyRelationshipMetadata(node: MutableNode, r: SessionRelationshipRecord): void {
  if (r.relationshipType === 'root') {
    if (node.relationshipType === 'root') node.label = 'main';
    return;
  }

  node.relationshipType = r.relationshipType;
  node.label = labelForRelationship(r);
  if (r.subagentType !== undefined) node.subagentType = r.subagentType;
  if (r.description !== undefined) node.description = r.description;
}

function addLegacySubagentGaps(state: GraphState, turns: readonly TurnRecord[]): void {
  for (const t of turns) {
    const sub = t.subagent;
    if (!sub?.agentId) continue;
    const id = canonicalId(state, sub.agentId);
    const node = ensureNode(
      state,
      id,
      sub.subagentType ?? '(unknown)',
      'subagent',
    );
    if (node.relationshipType === 'root') node.relationshipType = 'subagent';
    if (node.label === '(unknown)' && sub.subagentType !== undefined) {
      node.label = sub.subagentType;
    }
    if (node.subagentType === undefined && sub.subagentType !== undefined) {
      node.subagentType = sub.subagentType;
    }
    if (node.description === undefined && sub.description !== undefined) {
      node.description = sub.description;
    }
    if (state.parentByNode.has(id)) continue;
    const parentId = canonicalId(state, sub.parentAgentId ?? t.sessionId);
    state.parentByNode.set(id, parentId);
  }
}

function ensureTurnSessionRoots(state: GraphState, turns: readonly TurnRecord[]): void {
  for (const t of turns) {
    const id = canonicalId(state, t.sessionId);
    const node = ensureNode(state, id, 'main', 'root');
    if (node.relationshipType === 'root') node.label = 'main';
  }
  for (const parentId of state.parentByNode.values()) {
    ensureNode(state, parentId, parentId, 'root');
  }
}

function attachGraphChildren(state: GraphState): void {
  for (const [id, parentId] of state.parentByNode) {
    const node = state.nodeById.get(id);
    if (!node) continue;
    const resolvedParentId = resolveGraphParent(id, parentId, state.parentByNode);
    if (resolvedParentId === undefined) continue;
    const parent = state.nodeById.get(resolvedParentId);
    if (!parent) continue;
    if (!parent.children.includes(node)) parent.children.push(node);
  }
}

function collectAttachedChildIds(state: GraphState): Set<string> {
  const out = new Set<string>();
  for (const node of state.nodeById.values()) {
    for (const child of node.children) out.add(child.nodeId);
  }
  return out;
}

function attachTurnCosts(
  state: GraphState,
  turns: readonly TurnRecord[],
  pricing: PricingTable,
): void {
  const unresolvedByParent = new Map<string, MutableNode>();
  for (const t of turns) {
    const cost = costForTurn(t, pricing)?.total ?? 0;
    const sub = t.subagent;
    if (sub && !sub.agentId) {
      const parentId = canonicalId(state, t.sessionId);
      let unresolved = unresolvedByParent.get(parentId);
      if (!unresolved) {
        unresolved = ensureNode(
          state,
          `${parentId}:__unresolved`,
          '(unresolved)',
          'subagent',
        );
        state.parentByNode.set(unresolved.nodeId, parentId);
        const parent = state.nodeById.get(parentId);
        if (parent && !parent.children.includes(unresolved)) parent.children.push(unresolved);
        unresolvedByParent.set(parentId, unresolved);
      }
      addTurnToNode(state, unresolved.nodeId, t, cost);
      continue;
    }

    const id = sub?.agentId ? canonicalId(state, sub.agentId) : canonicalId(state, t.sessionId);
    ensureNode(state, id, sub?.subagentType ?? 'main', sub ? 'subagent' : 'root');
    addTurnToNode(state, id, t, cost);
  }
}

function addTurnToNode(
  state: GraphState,
  id: string,
  turn: TurnRecord,
  cost: number,
): void {
  const node = state.nodeById.get(id);
  if (!node) return;
  node.selfTurns++;
  node.selfCost += cost;
  if (turn.model) {
    let models = state.modelsByNode.get(id);
    if (!models) {
      models = new Set();
      state.modelsByNode.set(id, models);
    }
    models.add(turn.model);
  }
}

function finalizeTree(state: GraphState, root: MutableNode): void {
  const queue: Array<{ node: MutableNode; depth: number }> = [{ node: root, depth: 0 }];
  const seen = new Set<string>();
  while (queue.length > 0) {
    const { node, depth } = queue.shift()!;
    if (seen.has(node.nodeId)) continue;
    seen.add(node.nodeId);
    node.depth = depth;
    for (const child of node.children) {
      queue.push({ node: child, depth: depth + 1 });
    }
  }

  foldCumulative(root);
  assignModelArrays(state, root);
  sortTree(root);
}

function assignModelArrays(state: GraphState, root: MutableNode): void {
  const queue: MutableNode[] = [root];
  const seen = new Set<string>();
  while (queue.length > 0) {
    const node = queue.shift()!;
    if (seen.has(node.nodeId)) continue;
    seen.add(node.nodeId);
    const models = state.modelsByNode.get(node.nodeId);
    if (models) node.models = [...models].sort();
    for (const child of node.children) queue.push(child);
  }
}

function resolveGraphParent(
  id: string,
  parentId: string,
  parentByNode: Map<string, string>,
): string | undefined {
  if (parentId === id) return undefined;
  const seen = new Set<string>([id]);
  let cursor = parentId;
  while (parentByNode.has(cursor)) {
    if (seen.has(cursor)) return undefined;
    seen.add(cursor);
    cursor = parentByNode.get(cursor)!;
  }
  return parentId;
}

function buildSessionTree(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): SubagentTreeNode {
  const root: MutableNode = {
    nodeId: sessionId,
    label: 'main',
    relationshipType: 'root',
    models: [],
    selfTurns: 0,
    selfCost: 0,
    cumulativeTurns: 0,
    cumulativeCost: 0,
    depth: 0,
    children: [],
  };
  const byId = new Map<string, MutableNode>();
  byId.set(sessionId, root);

  const mainModels = new Set<string>();
  const modelsByNode = new Map<string, Set<string>>();
  modelsByNode.set(sessionId, mainModels);

  // Collect sidechain turns that arrived without a resolvable agentId so we
  // can still show their cost. Bucketed under a synthetic node named
  // '(unresolved)'.
  let unresolved: MutableNode | undefined;
  let unresolvedModels: Set<string> | undefined;

  for (const t of turns) {
    const cost = costForTurn(t, pricing)?.total ?? 0;
    if (!t.subagent) {
      root.selfTurns++;
      root.selfCost += cost;
      if (t.model) mainModels.add(t.model);
      continue;
    }
    const agentId = t.subagent.agentId;
    if (!agentId) {
      if (!unresolved) {
        unresolved = {
          nodeId: `${sessionId}:__unresolved`,
          label: '(unresolved)',
          relationshipType: 'subagent',
          models: [],
          selfTurns: 0,
          selfCost: 0,
          cumulativeTurns: 0,
          cumulativeCost: 0,
          depth: 1,
          children: [],
        };
        unresolvedModels = new Set<string>();
        root.children.push(unresolved);
      }
      unresolved.selfTurns++;
      unresolved.selfCost += cost;
      if (t.model && unresolvedModels) unresolvedModels.add(t.model);
      continue;
    }
    let node = byId.get(agentId);
    if (!node) {
      node = {
        nodeId: agentId,
        label: t.subagent.subagentType ?? '(unknown)',
        relationshipType: 'subagent',
        models: [],
        selfTurns: 0,
        selfCost: 0,
        cumulativeTurns: 0,
        cumulativeCost: 0,
        // depth is assigned during the parent-attach pass below.
        depth: -1,
        children: [],
      };
      if (t.subagent.subagentType !== undefined) node.subagentType = t.subagent.subagentType;
      if (t.subagent.description !== undefined) node.description = t.subagent.description;
      byId.set(agentId, node);
      modelsByNode.set(agentId, new Set<string>());
    } else {
      // Turns in the same invocation may fill in richer tree fields on
      // subsequent turns if we didn't have them on the first one. Backfill.
      if (node.subagentType === undefined && t.subagent.subagentType !== undefined) {
        node.subagentType = t.subagent.subagentType;
        if (node.label === '(unknown)') node.label = t.subagent.subagentType;
      }
      if (node.description === undefined && t.subagent.description !== undefined) {
        node.description = t.subagent.description;
      }
    }
    node.selfTurns++;
    node.selfCost += cost;
    if (t.model) modelsByNode.get(agentId)!.add(t.model);
  }

  // Attach each invocation node to its parent (by parentAgentId, falling
  // back to the session root when missing). Malformed data where a node is
  // its own parent or participates in a cycle gets redirected to the root
  // so foldCumulative / sortTree can't recurse infinitely.
  const parentByNode = new Map<string, string>();
  for (const t of turns) {
    if (!t.subagent?.agentId) continue;
    if (parentByNode.has(t.subagent.agentId)) continue;
    parentByNode.set(t.subagent.agentId, t.subagent.parentAgentId ?? sessionId);
  }
  for (const [id, parentId] of parentByNode) {
    const node = byId.get(id);
    if (!node) continue;
    const resolvedParentId = resolveParentOrRoot(id, parentId, parentByNode, sessionId);
    const parent = byId.get(resolvedParentId) ?? root;
    parent.children.push(node);
  }

  // Assign depth BFS from the root.
  const queue: Array<{ node: MutableNode; depth: number }> = [{ node: root, depth: 0 }];
  while (queue.length > 0) {
    const { node, depth } = queue.shift()!;
    node.depth = depth;
    for (const child of node.children) {
      queue.push({ node: child, depth: depth + 1 });
    }
  }

  // Finalize model arrays and fold cumulative cost/turns from leaves up.
  root.models = [...mainModels].sort();
  for (const [id, models] of modelsByNode) {
    const node = byId.get(id);
    if (node) node.models = [...models].sort();
  }
  if (unresolved && unresolvedModels) {
    unresolved.models = [...unresolvedModels].sort();
  }
  foldCumulative(root);
  // Sort children by cumulativeCost desc so rendering surfaces the expensive
  // branches first.
  sortTree(root);
  return root;
}

function foldCumulative(node: MutableNode): void {
  let cost = node.selfCost;
  let turns = node.selfTurns;
  for (const c of node.children) {
    foldCumulative(c);
    cost += c.cumulativeCost;
    turns += c.cumulativeTurns;
  }
  node.cumulativeCost = cost;
  node.cumulativeTurns = turns;
}

function sortTree(node: MutableNode): void {
  node.children.sort((a, b) => b.cumulativeCost - a.cumulativeCost);
  for (const c of node.children) sortTree(c);
}

// If `id` is its own parent, or walking up the parent chain revisits a node,
// the tree is malformed — redirect such a node straight to the session root
// so later recursion can't loop.
function resolveParentOrRoot(
  id: string,
  parentId: string,
  parentByNode: Map<string, string>,
  sessionId: string,
): string {
  if (parentId === id) return sessionId;
  const seen = new Set<string>([id]);
  let cursor = parentId;
  while (cursor !== sessionId) {
    if (seen.has(cursor)) return sessionId;
    seen.add(cursor);
    const next = parentByNode.get(cursor);
    if (next === undefined) return parentId;
    cursor = next;
  }
  return parentId;
}

export interface SubagentTypeStats {
  subagentType: string;
  invocations: number;
  turns: number;
  totalCost: number;
  medianCost: number;
  p95Cost: number;
  meanCost: number;
}

// Aggregate subagent invocations across sessions by `subagentType`, reporting
// per-invocation cost distribution. An "invocation" is the unique agentId
// within a session — all turns of the same spawned subagent count once.
export function aggregateSubagentTypeStats(
  turns: TurnRecord[],
  opts: BuildSubagentTreeOptions,
): SubagentTypeStats[] {
  const byInvocation = new Map<string, { type: string; turns: number; cost: number }>();
  for (const t of turns) {
    const sub = t.subagent;
    if (!sub?.agentId) continue;
    const type = sub.subagentType ?? '(unknown)';
    // Key on session+agentId so the same agentId in a different session can't
    // collide.
    const key = `${t.sessionId}:${sub.agentId}`;
    let inv = byInvocation.get(key);
    if (!inv) {
      inv = { type, turns: 0, cost: 0 };
      byInvocation.set(key, inv);
    } else if (inv.type === '(unknown)' && type !== '(unknown)') {
      inv.type = type;
    }
    inv.turns++;
    inv.cost += costForTurn(t, opts.pricing)?.total ?? 0;
  }
  const byType = new Map<string, number[]>();
  const totalsByType = new Map<string, { turns: number; total: number }>();
  for (const inv of byInvocation.values()) {
    let arr = byType.get(inv.type);
    if (!arr) {
      arr = [];
      byType.set(inv.type, arr);
    }
    arr.push(inv.cost);
    let sums = totalsByType.get(inv.type);
    if (!sums) {
      sums = { turns: 0, total: 0 };
      totalsByType.set(inv.type, sums);
    }
    sums.turns += inv.turns;
    sums.total += inv.cost;
  }
  const out: SubagentTypeStats[] = [];
  for (const [type, costs] of byType) {
    const totals = totalsByType.get(type)!;
    costs.sort((a, b) => a - b);
    out.push({
      subagentType: type,
      invocations: costs.length,
      turns: totals.turns,
      totalCost: totals.total,
      medianCost: percentile(costs, 0.5),
      p95Cost: percentile(costs, 0.95),
      meanCost: costs.length > 0 ? totals.total / costs.length : 0,
    });
  }
  return out.sort((a, b) => b.totalCost - a.totalCost);
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  // Nearest-rank with clamp at array bounds; matches the usual "summary table"
  // expectation (p50 of [1,2,3] = 2, p95 of a short list = max).
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[rank]!;
}
