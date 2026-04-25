import type {
  Coverage,
  Fidelity,
  FidelityClass,
  UsageGranularity,
} from './types.js';

// Coverage flags default to `false` — the safe answer to "do we know X?" when
// nothing has been asserted yet is "no". Each parser must explicitly opt every
// field it does cover into `true`. `Object.assign({}, EMPTY_COVERAGE, {…})` is
// the canonical way to build a coverage map without forgetting fields.
export const EMPTY_COVERAGE: Coverage = Object.freeze({
  hasInputTokens: false,
  hasOutputTokens: false,
  hasReasoningTokens: false,
  hasCacheReadTokens: false,
  hasCacheCreateTokens: false,
  hasToolCalls: false,
  hasToolResultEvents: false,
  hasSessionRelationships: false,
  hasRawContent: false,
});

// Fields required to call a record "full" fidelity for command-level purposes.
// Reasoning + cache-create are deliberately not in this list — Anthropic
// surfaces both, OpenAI surfaces reasoning but not cache-create, and we don't
// want to demote every Codex turn to "partial" just because it has no
// ephemeral cache-create concept. Commands that need those fields specifically
// (e.g. cache-aware projections) read the matching coverage flag directly.
const FULL_REQUIRED: ReadonlyArray<keyof Coverage> = [
  'hasInputTokens',
  'hasOutputTokens',
  'hasCacheReadTokens',
  'hasToolCalls',
  'hasToolResultEvents',
  'hasSessionRelationships',
];

const USAGE_REQUIRED: ReadonlyArray<keyof Coverage> = [
  'hasInputTokens',
  'hasOutputTokens',
];

// Derive the higher-level `FidelityClass` summary from a `granularity` +
// `coverage` pair. Pure function — no I/O, no mutation; safe to call from any
// layer (reader during construction, analyze for re-derivation, CLI for
// post-hoc filters).
//
//   - cost-only          → granularity says we only have a price, not tokens
//   - aggregate-only     → per-session totals; per-turn fields are estimates
//   - usage-only         → per-turn input/output but no tool-result chronology
//   - full               → meets FULL_REQUIRED above
//   - partial            → has *something* useful but less than usage-only
export function classifyFidelity(
  granularity: UsageGranularity,
  coverage: Coverage,
): FidelityClass {
  if (granularity === 'cost-only') return 'cost-only';
  if (granularity === 'per-session-aggregate') return 'aggregate-only';
  if (FULL_REQUIRED.every((k) => coverage[k])) return 'full';
  if (USAGE_REQUIRED.every((k) => coverage[k])) return 'usage-only';
  return 'partial';
}

// Convenience constructor — parsers build a `Coverage` object, pass it in
// alongside their declared granularity, and get a fully-populated `Fidelity`
// back with the derived class baked in.
export function makeFidelity(
  granularity: UsageGranularity,
  coverage: Coverage,
): Fidelity {
  return {
    granularity,
    coverage,
    class: classifyFidelity(granularity, coverage),
  };
}
