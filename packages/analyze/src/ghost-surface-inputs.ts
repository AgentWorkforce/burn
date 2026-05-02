import { readContent, type EnrichedTurn } from '@relayburn/ledger';
import type { SourceKind } from '@relayburn/reader';

import type { GhostSurfaceInputs } from './ghost-surface.js';
import type { PricingTable } from './pricing.js';

// Build the per-source observed-name set from a slice of EnrichedTurns. Used
// by `buildGhostSurfaceInputs` to anchor the ghost-surface detector against
// what actually showed up in this slice. Names are kept as-is and matched
// against file stems case-insensitively in the detector.
export function buildObservedNamesBySource(
  turns: ReadonlyArray<EnrichedTurn>,
): Map<SourceKind, Set<string>> {
  const out = new Map<SourceKind, Set<string>>();
  for (const t of turns) {
    let set = out.get(t.source);
    if (!set) {
      set = new Set<string>();
      out.set(t.source, set);
    }
    for (const call of t.toolCalls) {
      set.add(call.name);
      if (call.skillName) set.add(call.skillName);
    }
    if (t.subagent?.subagentType) {
      set.add(t.subagent.subagentType);
    }
  }
  return out;
}

export function buildSessionCountBySource(
  turns: ReadonlyArray<EnrichedTurn>,
): Map<SourceKind, number> {
  const seen = new Map<SourceKind, Set<string>>();
  for (const t of turns) {
    let set = seen.get(t.source);
    if (!set) {
      set = new Set<string>();
      seen.set(t.source, set);
    }
    set.add(t.sessionId);
  }
  const out = new Map<SourceKind, number>();
  for (const [source, set] of seen) out.set(source, set.size);
  return out;
}

// Pick a representative dollar-per-token rate for ghost-surface costing.
// User-installed surface rides in the CACHED prefix on every call after the
// first, so the cacheRead rate is the right basis. Pricing values are per
// million tokens, hence the / 1e6 conversion. We pick the cacheRead rate
// from the most-used model in the slice; ties go to the first-seen model.
// Falls back to 0 (which produces $0 cost but still surfaces ghosts) when
// no priced model is available.
export function pickRepresentativeCacheReadRate(
  turns: ReadonlyArray<EnrichedTurn>,
  pricing: PricingTable,
): number {
  const counts = new Map<string, number>();
  for (const t of turns) {
    counts.set(t.model, (counts.get(t.model) ?? 0) + 1);
  }
  let bestModel: string | undefined;
  let bestCount = -1;
  for (const [model, count] of counts) {
    if (count > bestCount && pricing[model]) {
      bestModel = model;
      bestCount = count;
    }
  }
  if (!bestModel) return 0;
  const rate = pricing[bestModel]!;
  return rate.cacheRead / 1_000_000;
}

// Build a per-source, per-session map of user-turn text strings for
// the slash-command observation pass. We only load content for sessions
// whose source has a slash-command notion (Claude commands, Codex prompts)
// — the OpenCode adapter doesn't consume `userTurnTextBySession` so the I/O
// would be wasted. The outer source key keeps each adapter's miner scoped
// to its own harness's text — without it, a Codex `/<stem>` literal match
// would fire against a Claude `<command-name>/<stem></command-name>` marker
// (and vice versa), falsely de-ghosting an identically-named surface in
// the other harness. Sessions whose sidecar is empty (`content.store=off`,
// pruned, or never captured) are silently absent from the inner map; the
// detector's `observedNames` hooks treat that as v1 fallback.
async function loadUserTurnTextBySession(
  turns: ReadonlyArray<EnrichedTurn>,
): Promise<Map<SourceKind, Map<string, string[]>>> {
  const out = new Map<SourceKind, Map<string, string[]>>();
  // Dedupe by (source, sessionId); only Claude / Codex sessions need mining.
  const sessionsBySource = new Map<SourceKind, Set<string>>();
  for (const t of turns) {
    if (t.source !== 'claude-code' && t.source !== 'codex') continue;
    let set = sessionsBySource.get(t.source);
    if (!set) {
      set = new Set<string>();
      sessionsBySource.set(t.source, set);
    }
    set.add(t.sessionId);
  }
  for (const [source, sessionIds] of sessionsBySource) {
    const inner = new Map<string, string[]>();
    for (const sessionId of sessionIds) {
      const records = await readContent({ sessionId });
      if (records.length === 0) continue;
      const texts: string[] = [];
      for (const rec of records) {
        if (rec.role !== 'user' || rec.kind !== 'text') continue;
        if (typeof rec.text !== 'string' || rec.text.length === 0) continue;
        texts.push(rec.text);
      }
      if (texts.length > 0) inner.set(sessionId, texts);
    }
    if (inner.size > 0) out.set(source, inner);
  }
  return out;
}

export async function buildGhostSurfaceInputs(
  turns: ReadonlyArray<EnrichedTurn>,
  pricing: PricingTable,
): Promise<GhostSurfaceInputs> {
  const userTurnTextBySession = await loadUserTurnTextBySession(turns);
  const inputs: GhostSurfaceInputs = {
    observedNamesBySource: buildObservedNamesBySource(turns),
    sessionCountBySource: buildSessionCountBySource(turns),
    dollarPerToken: pickRepresentativeCacheReadRate(turns, pricing),
  };
  // Only attach when non-empty — keeps the v1 fallback path clean for
  // sessions where the sidecar was unavailable.
  if (userTurnTextBySession.size > 0) {
    inputs.userTurnTextBySession = userTurnTextBySession;
  }
  return inputs;
}
