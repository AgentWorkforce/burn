import { classifyFidelity } from '@relayburn/reader';
import type {
  Coverage,
  Fidelity,
  FidelityClass,
  TurnRecord,
  UsageGranularity,
} from '@relayburn/reader';

// What every command needs to honestly describe a slice of turns:
//   - how many turns landed in each FidelityClass
//   - how many turns are missing each individual coverage field
// The `unknown` bucket counts records that pre-date the fidelity field on
// `TurnRecord` (older ledger writers, foreign sources). Treat them as
// best-effort full fidelity for backward compatibility, but expose the count
// so callers can show the gap if they care.
export interface FidelitySummary {
  total: number;
  byClass: Record<FidelityClass, number>;
  byGranularity: Record<UsageGranularity, number>;
  missingCoverage: Record<keyof Coverage, number>;
  // Records with no `fidelity` field at all — emitted by ledger writers from
  // before issue #41. Counted separately so we don't pretend they're "full".
  unknown: number;
}

export function emptyFidelitySummary(): FidelitySummary {
  return {
    total: 0,
    byClass: {
      full: 0,
      'usage-only': 0,
      'aggregate-only': 0,
      'cost-only': 0,
      partial: 0,
    },
    byGranularity: {
      'per-turn': 0,
      'per-message': 0,
      'per-session-aggregate': 0,
      'cost-only': 0,
    },
    missingCoverage: {
      hasInputTokens: 0,
      hasOutputTokens: 0,
      hasReasoningTokens: 0,
      hasCacheReadTokens: 0,
      hasCacheCreateTokens: 0,
      hasToolCalls: 0,
      hasToolResultEvents: 0,
      hasSessionRelationships: 0,
      hasRawContent: 0,
    },
    unknown: 0,
  };
}

// Walk a slice of turns and emit a `FidelitySummary`. Pure aggregation — no
// I/O, no caching, safe to call repeatedly. Callers serialize the result for
// JSON output or render the relevant counts inline.
export function summarizeFidelity(
  turns: ReadonlyArray<Pick<TurnRecord, 'fidelity'>>,
): FidelitySummary {
  const out = emptyFidelitySummary();
  out.total = turns.length;
  for (const t of turns) {
    const f = t.fidelity;
    if (!f) {
      out.unknown++;
      continue;
    }
    // Trust whatever `class` was written, but re-derive when granularity +
    // coverage say something different — the older serializer might be lying.
    const cls: FidelityClass = f.class ?? classifyFidelity(f.granularity, f.coverage);
    out.byClass[cls]++;
    out.byGranularity[f.granularity]++;
    // Count records *missing* each field (so a non-zero number always means
    // "this many turns lack X"). Easier to read than hasX counts when the
    // overwhelming common case is "everything has X".
    for (const key of Object.keys(out.missingCoverage) as Array<keyof Coverage>) {
      if (!f.coverage[key]) out.missingCoverage[key]++;
    }
  }
  return out;
}

// Convenience predicate for the "default exclude aggregate-only / cost-only"
// filtering pattern flagged in #41 for `burn compare` and friends. Records
// without fidelity are treated as best-effort full (older ledger writers
// pre-#41) — a strict mode that drops unknown is a separate decision the
// caller can layer on top.
const FIDELITY_ORDER: ReadonlyArray<FidelityClass> = [
  'cost-only',
  'aggregate-only',
  'partial',
  'usage-only',
  'full',
];

export function hasMinimumFidelity(
  fidelity: Fidelity | undefined,
  minimum: FidelityClass,
): boolean {
  if (!fidelity) return true;
  const need = FIDELITY_ORDER.indexOf(minimum);
  const have = FIDELITY_ORDER.indexOf(fidelity.class);
  return have >= need;
}
