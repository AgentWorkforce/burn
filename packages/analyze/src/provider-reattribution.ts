// Synthetic provider detection / cross-collector reattribution.
//
// Synthetic.new is not a traditional collector — it's a router that lets users
// invoke models from various underlying providers via prefixed model IDs
// (e.g. `hf:deepseek-ai/...`, `accounts/fireworks/models/...`). When a Claude
// Code or OpenCode session uses a Synthetic-routed model, the model ID in the
// session log carries a Synthetic-style prefix but the session data lives in
// the harness's normal store. To attribute that traffic correctly burn applies
// rule-based reattribution at query time — never at ledger-write time, so the
// raw model string stays revisitable as the rule set evolves.
//
// This is the extension point for future aggregator detectors (OpenRouter,
// etc.). Add a `ProviderRule` to `DEFAULT_RULES` and the resolver picks it up.
//
// Tracked in https://github.com/AgentWorkforce/burn/issues/31.
//
// Pattern coverage today (mirrors tokscale's `synthetic.rs`):
//   - `hf:*`                       — HuggingFace-prefixed models routed via Synthetic
//   - `accounts/fireworks/models/*` — Fireworks-prefixed Synthetic models
//   - `synthetic/*`                — explicit Synthetic-prefixed
//
// Deferred (see issue #31):
//   - Octofriend SQLite fallback — defer until Octofriend persists tokens.
//   - OpenRouter / other aggregator prefixes — same scaffolding, future PR.

export interface ProviderResolution {
  /** Resolved provider label (e.g. `'synthetic'`). `undefined` when the rule
   * set has nothing to say about this model — callers fall back to whatever
   * provider their data source already implies. */
  provider?: string;
  /** Model identifier with the routing prefix stripped, suitable for pricing
   * lookup (e.g. `hf:deepseek-ai/deepseek-r1` → `deepseek-r1`). Equal to the
   * input string when no rule matched. */
  normalizedModel: string;
  /** Name of the rule that matched, or `undefined` if none did. Surfaced in
   * tests / debug output so it's clear which prefix triggered the rewrite. */
  matchedRule?: string;
}

export interface ProviderRule {
  /** Stable identifier; appears in `ProviderResolution.matchedRule` and is
   * used to dedupe / replace rules when callers extend `DEFAULT_RULES`. */
  name: string;
  /** Provider label to assign when the rule fires. */
  provider: string;
  /** Either a literal prefix (matched via `startsWith`) or a regex with at
   * least one capturing group whose first capture is the normalized model. */
  pattern: string | RegExp;
}

// Rule order matters: the first match wins. More specific prefixes
// (`accounts/fireworks/models/`) come before short ones (`hf:`) so a future
// `hf:accounts/...`-style oddity wouldn't be misattributed.
export const DEFAULT_RULES: readonly ProviderRule[] = [
  {
    name: 'synthetic-fireworks',
    provider: 'synthetic',
    pattern: /^accounts\/fireworks\/models\/(.+)$/,
  },
  {
    name: 'synthetic-explicit',
    provider: 'synthetic',
    pattern: /^synthetic\/(.+)$/,
  },
  {
    name: 'synthetic-huggingface',
    provider: 'synthetic',
    // `hf:deepseek-ai/deepseek-r1-distill` → strip `hf:` and the org segment
    // so the residual matches the bare model id used by models.dev pricing.
    pattern: /^hf:(?:[^/]+\/)?(.+)$/,
  },
] as const;

/**
 * Resolve a provider + normalized model id from a raw model string.
 *
 * Pure / deterministic. Safe to call per-turn at query time.
 */
export function resolveProvider(
  model: string,
  rules: readonly ProviderRule[] = DEFAULT_RULES,
): ProviderResolution {
  for (const rule of rules) {
    const matched = applyRule(model, rule);
    if (matched !== null) {
      return {
        provider: rule.provider,
        normalizedModel: matched,
        matchedRule: rule.name,
      };
    }
  }
  return { normalizedModel: model };
}

function applyRule(model: string, rule: ProviderRule): string | null {
  if (typeof rule.pattern === 'string') {
    return model.startsWith(rule.pattern) ? model.slice(rule.pattern.length) : null;
  }
  const m = rule.pattern.exec(model);
  if (!m) return null;
  // First capture group is the normalized model; if a rule omits a capture
  // group treat the remainder after the match as the normalized id.
  const captured = m[1];
  return captured ?? model.slice(m[0].length);
}
