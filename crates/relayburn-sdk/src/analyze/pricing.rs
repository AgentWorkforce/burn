//! Per-million-token pricing tables.
//!
//! The bundled `models.dev.json` snapshot ships embedded via `include_str!` so
//! the binary stays self-contained and `load_builtin_pricing` performs no I/O.
//! `load_pricing` accepts an optional override path that, when present,
//! shallow-merges over the builtin (override entries win on collision).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// How a model's reasoning tokens should be priced.
///
/// - `IncludedInOutput`: the harness/source already counts reasoning tokens
///   inside `usage.output`, so `usage.reasoning` is informational only and
///   must NOT be billed on top. Codex transcripts behave this way.
/// - `Separate`: the model has a distinct reasoning tariff (`cost.reasoning`
///   in the `models.dev` snapshot). Bill `usage.reasoning` at that tariff.
/// - `SameAsOutput`: `usage.output` and `usage.reasoning` are non-overlapping
///   token buckets and there is no distinct reasoning tariff. Bill
///   `usage.reasoning` at the output rate. Anthropic Claude transcripts are
///   the canonical example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    IncludedInOutput,
    Separate,
    SameAsOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Per-million reasoning-token tariff. Set iff `reasoning_mode == Separate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    pub reasoning_mode: ReasoningMode,
}

pub type PricingTable = HashMap<String, ModelCost>;

#[derive(Debug, Default, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    #[serde(default)]
    reasoning: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevProvider {
    // `IndexMap` preserves JSON insertion order so iteration over the model
    // map matches the TS `Object.entries` walk; this is what makes
    // duplicate-id resolution deterministic (last entry in file order wins).
    #[serde(default)]
    models: Option<IndexMap<String, ModelsDevModel>>,
}

// Same reason as `ModelsDevProvider::models`: providers iterate in file
// order so a duplicate model id encountered under a later provider block
// overwrites the earlier one, matching the TS `Object.values(root)` walk.
type ModelsDevRoot = IndexMap<String, ModelsDevProvider>;

/// Bundled `models.dev.json` snapshot. Refreshed via `pnpm run pricing:update`,
/// which writes through to the SDK crate's `data/` copy. Vendoring inside the
/// crate is required so `cargo package` / `cargo publish --dry-run` can verify
/// the tarball without network access.
const BUILTIN_PRICING_JSON: &str = include_str!("../../data/models.dev.json");

/// Parsed `BUILTIN_PRICING_JSON`. Parsing the snapshot allocates a
/// `HashMap` of several hundred entries, and `load_builtin_pricing` is on the
/// hot path of multiple SDK verbs that each used to re-parse it.
static BUILTIN_PRICING: LazyLock<PricingTable> = LazyLock::new(|| {
    parse_pricing(BUILTIN_PRICING_JSON).expect("bundled models.dev.json must parse")
});

/// Load the bundled `models.dev` snapshot. No I/O — the JSON is embedded at
/// compile time via `include_str!` and parsed once into a `LazyLock` cache.
pub fn load_builtin_pricing() -> PricingTable {
    BUILTIN_PRICING.clone()
}

/// Load pricing, optionally merging an override file over the builtin. When
/// `override_path` is `Some` and the file exists + parses, its entries shadow
/// builtin entries on key collision (TS `{ ...builtin, ...user }` semantics).
/// Any read or parse error on the override silently falls back to the builtin
/// — matches the TS `try/catch` in `loadPricing`.
pub fn load_pricing(override_path: Option<&Path>) -> PricingTable {
    let mut table = load_builtin_pricing();
    if let Some(path) = override_path {
        if let Ok(user) = load_from_file(path) {
            for (k, v) in user {
                table.insert(k, v);
            }
        }
    }
    table
}

fn load_from_file(path: &Path) -> io::Result<PricingTable> {
    let raw = fs::read_to_string(path)?;
    parse_pricing(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn parse_pricing(raw: &str) -> serde_json::Result<PricingTable> {
    let parsed: ModelsDevRoot = serde_json::from_str(raw)?;
    Ok(flatten(&parsed))
}

/// Flatten a nested `provider → model → cost` map into the flat
/// `model_id → ModelCost` table burn uses for lookup. Skips entries that lack
/// either `input` or `output` — matches the TS guard so we don't surface
/// half-priced models.
fn flatten(root: &ModelsDevRoot) -> PricingTable {
    let mut out = PricingTable::new();
    for provider in root.values() {
        let Some(models) = provider.models.as_ref() else {
            continue;
        };
        for (id, model) in models {
            let Some(cost) = model.cost.as_ref() else {
                continue;
            };
            let (Some(input), Some(output)) = (cost.input, cost.output) else {
                continue;
            };
            let has_reasoning = cost.reasoning.is_some();
            let entry = ModelCost {
                input,
                output,
                cache_read: cost.cache_read.unwrap_or(0.0),
                // Mirrors the TS `cost.cache_write ?? cost.input` fallback so
                // models that don't publish a cache-write rate get billed at
                // the input rate for cache creation.
                cache_write: cost.cache_write.unwrap_or(input),
                reasoning: cost.reasoning,
                reasoning_mode: if has_reasoning {
                    ReasoningMode::Separate
                } else {
                    ReasoningMode::SameAsOutput
                },
            };
            out.insert(id.clone(), entry);
        }
    }
    out
}

/// Public flatten helper, useful for callers that already hold a parsed
/// `models.dev`-shaped JSON value.
pub fn flatten_value(value: &serde_json::Value) -> serde_json::Result<PricingTable> {
    let root: ModelsDevRoot = serde_json::from_value(value.clone())?;
    Ok(flatten(&root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_snapshot_parses_and_has_anthropic_models() {
        let table = load_builtin_pricing();
        assert!(table.contains_key("claude-opus-4-7"), "opus-4-7 present");
        assert!(
            table.contains_key("claude-sonnet-4-6"),
            "sonnet-4-6 present"
        );
        assert!(table.contains_key("claude-haiku-4-5"), "haiku-4-5 present");
    }

    #[test]
    fn flatten_preserves_separate_reasoning_tariff() {
        let raw = r#"{
            "acme": {
                "id": "acme",
                "models": {
                    "reasoner-v1": {
                        "id": "reasoner-v1",
                        "cost": {
                            "input": 0.7,
                            "output": 2.8,
                            "reasoning": 8.4,
                            "cache_read": 0.07,
                            "cache_write": 0.7
                        }
                    }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let entry = table.get("reasoner-v1").expect("flattened entry");
        assert_eq!(entry.input, 0.7);
        assert_eq!(entry.output, 2.8);
        assert_eq!(entry.reasoning, Some(8.4));
        assert_eq!(entry.cache_read, 0.07);
        assert_eq!(entry.cache_write, 0.7);
        assert_eq!(entry.reasoning_mode, ReasoningMode::Separate);
    }

    #[test]
    fn flatten_defaults_reasoning_mode_to_same_as_output() {
        let raw = r#"{
            "acme": {
                "id": "acme",
                "models": {
                    "plain-v1": {
                        "id": "plain-v1",
                        "cost": { "input": 1, "output": 2 }
                    }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let entry = table.get("plain-v1").unwrap();
        assert_eq!(entry.reasoning_mode, ReasoningMode::SameAsOutput);
        assert_eq!(entry.reasoning, None);
        // cache_write falls back to input when omitted.
        assert_eq!(entry.cache_write, 1.0);
    }

    #[test]
    fn flatten_resolves_duplicate_model_ids_with_last_wins_file_order() {
        // `models.dev.json` lists ~1900 model IDs that appear under multiple
        // providers (e.g. `claude-sonnet-4-6` under both `anthropic` and
        // `nano-gpt`). The TS path iterates `Object.values(root)` /
        // `Object.entries(models)` in insertion order and lets the last
        // assignment to `out[id]` win. We must do the same so duplicate-id
        // pricing is deterministic across runs.
        let raw = r#"{
            "first":  { "models": { "shared": { "cost": { "input": 1, "output": 2 } } } },
            "second": { "models": { "shared": { "cost": { "input": 9, "output": 8 } } } }
        }"#;
        // Run repeatedly; each run must surface the second-provider entry.
        for _ in 0..10 {
            let table = parse_pricing(raw).unwrap();
            let entry = table.get("shared").unwrap();
            assert_eq!(entry.input, 9.0);
            assert_eq!(entry.output, 8.0);
        }
    }

    #[test]
    fn flatten_skips_models_without_input_or_output() {
        let raw = r#"{
            "acme": {
                "models": {
                    "broken": { "cost": { "input": 1 } },
                    "ok":     { "cost": { "input": 1, "output": 2 } }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        assert!(!table.contains_key("broken"));
        assert!(table.contains_key("ok"));
    }

    #[test]
    fn builtin_snapshot_keeps_at_least_one_separate_tariff_model() {
        let table = load_builtin_pricing();
        let separate: Vec<_> = table
            .values()
            .filter(|m| m.reasoning_mode == ReasoningMode::Separate)
            .collect();
        assert!(
            !separate.is_empty(),
            "expected at least one separate-tariff model in the bundled snapshot",
        );
        for m in separate {
            assert!(m.reasoning.is_some());
        }
    }

    #[test]
    fn load_pricing_falls_back_to_builtin_on_missing_override() {
        let table = load_pricing(Some(Path::new("/nonexistent/path/models.json")));
        assert!(table.contains_key("claude-opus-4-7"));
    }

    #[test]
    fn load_pricing_merges_override_over_builtin() {
        let dir = std::env::temp_dir();
        let override_path = dir.join(format!(
            "relayburn-analyze-test-{}.json",
            std::process::id()
        ));
        // Override `claude-opus-4-7` with synthetic numbers; assert the
        // override wins on key collision and a fresh key is added.
        let raw = r#"{
            "test": {
                "models": {
                    "claude-opus-4-7": { "cost": { "input": 999, "output": 999 } },
                    "fresh-model":     { "cost": { "input": 1,   "output": 2   } }
                }
            }
        }"#;
        fs::write(&override_path, raw).unwrap();
        let table = load_pricing(Some(&override_path));
        let _ = fs::remove_file(&override_path);

        assert_eq!(table.get("claude-opus-4-7").unwrap().input, 999.0);
        assert!(table.contains_key("fresh-model"));
        // Other builtin entries are still present.
        assert!(table.contains_key("claude-sonnet-4-6"));
    }
}
