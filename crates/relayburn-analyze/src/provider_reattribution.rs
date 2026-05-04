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

use regex::Regex;

use std::sync::OnceLock;

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

struct ProviderRule {
    name: &'static str,
    provider: &'static str,
    /// Regex with a single capturing group — the first capture is the
    /// normalized model id.
    pattern: &'static str,
}

// Rule order matters: the first match wins. More specific prefixes
// (`accounts/fireworks/models/`) come before short ones (`hf:`) so a future
// `hf:accounts/...`-style oddity wouldn't be misattributed.
const DEFAULT_RULES: &[ProviderRule] = &[
    ProviderRule {
        name: "synthetic-fireworks",
        provider: "synthetic",
        pattern: r"^accounts/fireworks/models/(.+)$",
    },
    ProviderRule {
        name: "synthetic-explicit",
        provider: "synthetic",
        pattern: r"^synthetic/(.+)$",
    },
    ProviderRule {
        name: "synthetic-huggingface",
        // `hf:deepseek-ai/deepseek-r1-distill` → strip `hf:` and the org
        // segment so the residual matches the bare model id used by
        // models.dev pricing.
        provider: "synthetic",
        pattern: r"^hf:(?:[^/]+/)?(.+)$",
    },
];

fn compiled_regexes() -> &'static [Regex] {
    static CELL: OnceLock<Vec<Regex>> = OnceLock::new();
    CELL.get_or_init(|| {
        DEFAULT_RULES
            .iter()
            .map(|rule| Regex::new(rule.pattern).expect("default rule regex must compile"))
            .collect()
    })
}

/// Resolve a provider + normalized model id from a raw model string. Pure /
/// deterministic; safe to call per-turn at query time.
pub fn resolve_provider(model: &str) -> ProviderResolution {
    let regexes = compiled_regexes();
    for (idx, rule) in DEFAULT_RULES.iter().enumerate() {
        if let Some(caps) = regexes[idx].captures(model) {
            // First capture group is the normalized model; the default rule
            // set always provides one, but we fall back to the residual
            // after the whole match to mirror the TS guard.
            let normalized = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| {
                    let whole = caps.get(0).unwrap().as_str();
                    model[whole.len()..].to_string()
                });
            return ProviderResolution {
                provider: Some(rule.provider.to_string()),
                normalized_model: normalized,
                matched_rule: Some(rule.name.to_string()),
            };
        }
    }
    ProviderResolution {
        provider: None,
        normalized_model: model.to_string(),
        matched_rule: None,
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
    fn no_match_returns_input_unchanged() {
        let r = resolve_provider("claude-sonnet-4-6");
        assert_eq!(r.provider, None);
        assert_eq!(r.normalized_model, "claude-sonnet-4-6");
        assert_eq!(r.matched_rule, None);
    }
}
