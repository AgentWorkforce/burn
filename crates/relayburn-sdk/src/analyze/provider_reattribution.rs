//! Synthetic-provider detection / cross-collector reattribution. Rust port of
//! `packages/analyze/src/provider-reattribution.ts`.
//!
//! Synthetic.new is a router that lets users invoke models from various
//! underlying providers via prefixed model IDs (e.g. `hf:deepseek-ai/...`,
//! `accounts/fireworks/models/...`). When a Claude Code or OpenCode session
//! uses a Synthetic-routed model, the model ID in the session log carries a
//! Synthetic-style prefix but the session data lives in the harness's normal
//! store. To attribute that traffic correctly burn applies rule-based
//! reattribution at query time — never at ledger-write time, so the raw model
//! string stays revisitable as the rule set evolves.
//!
//! This is the extension point for future aggregator detectors (OpenRouter,
//! etc.). Add a [`ProviderRule`] to a callsite-specific rule set or extend the
//! defaults via [`extend_default_rules`] and the resolver picks it up.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResolution {
    /// Resolved provider label (e.g. `"synthetic"`). `None` when no rule
    /// matched — callers fall back to whatever provider their data source
    /// already implies.
    pub provider: Option<String>,
    /// Model identifier with the routing prefix stripped, suitable for
    /// pricing lookup. Equal to the input string when no rule matched.
    pub normalized_model: String,
    /// Name of the rule that fired, or `None` if none did.
    pub matched_rule: Option<String>,
}

/// Pattern used by a [`ProviderRule`]. Mirrors the TS `string | RegExp`
/// pattern field: literal prefixes use `Prefix` and behave like
/// `String.prototype.startsWith`; regex patterns must include a single
/// capturing group whose first capture is the normalized model id.
#[derive(Debug, Clone)]
pub enum ProviderPattern {
    Prefix(String),
    Regex(Regex),
}

#[derive(Debug, Clone)]
pub struct ProviderRule {
    /// Stable identifier; appears in [`ProviderResolution::matched_rule`] and
    /// is used to dedupe / replace rules when callers extend
    /// [`default_rules`].
    pub name: String,
    /// Provider label to assign when the rule fires.
    pub provider: String,
    /// Either a literal prefix (matched via `starts_with`) or a regex with
    /// at least one capturing group whose first capture is the normalized
    /// model.
    pub pattern: ProviderPattern,
}

impl ProviderRule {
    /// Build a literal-prefix rule. The rule fires when the model string
    /// starts with `prefix`; the normalized model is the residual after the
    /// prefix.
    pub fn prefix(
        name: impl Into<String>,
        provider: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            provider: provider.into(),
            pattern: ProviderPattern::Prefix(prefix.into()),
        }
    }

    /// Build a regex-pattern rule. Returns an error if `pattern` doesn't
    /// compile.
    pub fn regex(
        name: impl Into<String>,
        provider: impl Into<String>,
        pattern: &str,
    ) -> Result<Self, regex::Error> {
        Ok(Self {
            name: name.into(),
            provider: provider.into(),
            pattern: ProviderPattern::Regex(Regex::new(pattern)?),
        })
    }
}

// Rule order matters: the first match wins. More specific prefixes
// (`accounts/fireworks/models/`) come before short ones (`hf:`) so a future
// `hf:accounts/...`-style oddity wouldn't be misattributed.
//
// Mirrors `DEFAULT_RULES` in `packages/analyze/src/provider-reattribution.ts`
// byte-for-byte (same names, same providers, same patterns, same order).
static DEFAULT_RULES_INNER: LazyLock<Vec<ProviderRule>> = LazyLock::new(|| {
    vec![
        ProviderRule {
            name: "synthetic-fireworks".into(),
            provider: "synthetic".into(),
            pattern: ProviderPattern::Regex(
                Regex::new(r"^accounts/fireworks/models/(.+)$").expect("default regex compiles"),
            ),
        },
        ProviderRule {
            name: "synthetic-explicit".into(),
            provider: "synthetic".into(),
            pattern: ProviderPattern::Regex(
                Regex::new(r"^synthetic/(.+)$").expect("default regex compiles"),
            ),
        },
        ProviderRule {
            name: "synthetic-huggingface".into(),
            // `hf:deepseek-ai/deepseek-r1-distill` → strip `hf:` and the org
            // segment so the residual matches the bare model id used by
            // models.dev pricing.
            provider: "synthetic".into(),
            pattern: ProviderPattern::Regex(
                Regex::new(r"^hf:(?:[^/]+/)?(.+)$").expect("default regex compiles"),
            ),
        },
    ]
});

/// Default rule set shipped with the analyzer — same prefixes, providers and
/// ordering as the TS [`DEFAULT_RULES`] export.
pub fn default_rules() -> &'static [ProviderRule] {
    DEFAULT_RULES_INNER.as_slice()
}

/// Convenience helper: returns a fresh `Vec<ProviderRule>` containing the
/// defaults followed by `extra`. Mirrors the TS spread idiom
/// `[...DEFAULT_RULES, customRule]` used by callers extending the rule set.
pub fn extend_default_rules<I>(extra: I) -> Vec<ProviderRule>
where
    I: IntoIterator<Item = ProviderRule>,
{
    let mut rules: Vec<ProviderRule> = default_rules().to_vec();
    rules.extend(extra);
    rules
}

/// Resolve a provider + normalized model id from a raw model string against
/// the default rule set. Pure / deterministic; safe to call per-turn at
/// query time.
pub fn resolve_provider(model: &str) -> ProviderResolution {
    resolve_provider_with_rules(model, default_rules())
}

/// Like [`resolve_provider`] but uses an explicit rule set — used by the
/// public analyzer surface to thread `AggregateByProviderOptions::rules`
/// through, and by tests to exercise extension scenarios.
pub fn resolve_provider_with_rules(model: &str, rules: &[ProviderRule]) -> ProviderResolution {
    for rule in rules {
        if let Some(normalized) = apply_rule(model, rule) {
            return ProviderResolution {
                provider: Some(rule.provider.clone()),
                normalized_model: normalized,
                matched_rule: Some(rule.name.clone()),
            };
        }
    }
    ProviderResolution {
        provider: None,
        normalized_model: model.to_string(),
        matched_rule: None,
    }
}

fn apply_rule(model: &str, rule: &ProviderRule) -> Option<String> {
    match &rule.pattern {
        ProviderPattern::Prefix(prefix) => {
            if model.starts_with(prefix.as_str()) {
                Some(model[prefix.len()..].to_string())
            } else {
                None
            }
        }
        ProviderPattern::Regex(re) => {
            let caps = re.captures(model)?;
            // First capture group is the normalized model; if a rule omits a
            // capture group treat the remainder after the match as the
            // normalized id (mirrors the TS `m[1] ?? model.slice(m[0].length)`
            // guard).
            Some(if let Some(cap) = caps.get(1) {
                cap.as_str().to_string()
            } else {
                let whole = caps.get(0).expect("regex match has full text");
                model[whole.end()..].to_string()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fireworks_prefix_strips_to_bare_model() {
        let r = resolve_provider("accounts/fireworks/models/llama-3.1-405b");
        assert_eq!(r.provider.as_deref(), Some("synthetic"));
        assert_eq!(r.normalized_model, "llama-3.1-405b");
        assert_eq!(r.matched_rule.as_deref(), Some("synthetic-fireworks"));
    }

    #[test]
    fn synthetic_explicit_prefix_strips() {
        let r = resolve_provider("synthetic/qwen3-coder");
        assert_eq!(r.provider.as_deref(), Some("synthetic"));
        assert_eq!(r.normalized_model, "qwen3-coder");
        assert_eq!(r.matched_rule.as_deref(), Some("synthetic-explicit"));
    }

    #[test]
    fn hf_prefix_strips_org_segment() {
        let r = resolve_provider("hf:deepseek-ai/deepseek-r1-distill");
        assert_eq!(r.provider.as_deref(), Some("synthetic"));
        assert_eq!(r.normalized_model, "deepseek-r1-distill");
        assert_eq!(r.matched_rule.as_deref(), Some("synthetic-huggingface"));
    }

    #[test]
    fn hf_with_no_org_segment_routes_to_synthetic() {
        let r = resolve_provider("hf:llama-3-70b");
        assert_eq!(r.provider.as_deref(), Some("synthetic"));
        assert_eq!(r.normalized_model, "llama-3-70b");
    }

    #[test]
    fn no_match_returns_input_unchanged() {
        let r = resolve_provider("claude-sonnet-4-6");
        assert_eq!(r.provider, None);
        assert_eq!(r.normalized_model, "claude-sonnet-4-6");
        assert_eq!(r.matched_rule, None);
    }

    #[test]
    fn anthropic_provider_prefix_is_left_to_cost_fallback() {
        // The reattribution layer is intentionally narrow: it does not own
        // the generic `provider/model` strip — that's still cost.rs's job.
        let r = resolve_provider("anthropic/claude-sonnet-4-6");
        assert_eq!(r.provider, None);
        assert_eq!(r.normalized_model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn first_matching_rule_wins() {
        // Sanity check: explicit `synthetic/` should not also accidentally
        // match `hf:`, and the fireworks rule should win over any later rule
        // that could swallow it.
        let r = resolve_provider("accounts/fireworks/models/synthetic/foo");
        assert_eq!(r.matched_rule.as_deref(), Some("synthetic-fireworks"));
        assert_eq!(r.normalized_model, "synthetic/foo");
    }

    #[test]
    fn accepts_a_custom_rule_set() {
        let rules = extend_default_rules([ProviderRule::prefix(
            "openrouter",
            "openrouter",
            "openrouter/",
        )]);
        let r = resolve_provider_with_rules("openrouter/anthropic/claude-sonnet-4-6", &rules);
        assert_eq!(r.provider.as_deref(), Some("openrouter"));
        assert_eq!(r.normalized_model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn default_rules_table_matches_ts_one_for_one() {
        // Regression guard for the conformance gate in #267: every default
        // rule must resolve to the documented provider for a representative
        // model id, in the documented order.
        let rules = default_rules();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].name, "synthetic-fireworks");
        assert_eq!(rules[0].provider, "synthetic");
        assert_eq!(
            resolve_provider("accounts/fireworks/models/deepseek-r1")
                .matched_rule
                .as_deref(),
            Some("synthetic-fireworks"),
        );
        assert_eq!(rules[1].name, "synthetic-explicit");
        assert_eq!(rules[1].provider, "synthetic");
        assert_eq!(
            resolve_provider("synthetic/deepseek-r1-0528")
                .matched_rule
                .as_deref(),
            Some("synthetic-explicit"),
        );
        assert_eq!(rules[2].name, "synthetic-huggingface");
        assert_eq!(rules[2].provider, "synthetic");
        assert_eq!(
            resolve_provider("hf:deepseek-ai/deepseek-r1-distill")
                .matched_rule
                .as_deref(),
            Some("synthetic-huggingface"),
        );
    }
}
