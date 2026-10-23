//! Per-million-token pricing tables.
//!
//! The bundled `models.dev.json` snapshot ships embedded via `include_str!` so
//! the binary stays self-contained and `load_builtin_pricing` performs no I/O.
//! `load_pricing` accepts an optional override path that, when present,
//! shallow-merges over the builtin (override entries win on collision).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::reader::SourceKind;

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
    /// Per-million tariff for cache writes with the default (5-minute) TTL.
    pub cache_write: f64,
    /// Per-million tariff for cache writes with a 1-hour TTL.
    pub cache_write_1h: f64,
    /// Per-million reasoning-token tariff. Set iff `reasoning_mode == Separate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    pub reasoning_mode: ReasoningMode,
    /// Higher tariffs selected when the turn's input context exceeds the
    /// tier threshold. Ordered by ascending threshold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tiers: Vec<ModelCostTier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    pub context_tokens: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
}

/// Per-model tariffs resolved from a models.dev-shaped snapshot, plus the
/// tariffs of providers that bill a harness's sessions themselves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PricingTable {
    /// One tariff per model id, chosen by [`resolve_tariff`].
    models: HashMap<String, ModelCost>,
    /// provider id → model id → that provider's own tariff, kept only for
    /// [`BILLING_PROVIDERS`].
    billed: HashMap<String, HashMap<String, ModelCost>>,
}

impl PricingTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The resolved tariff for `model`, independent of who served it.
    pub fn get(&self, model: &str) -> Option<&ModelCost> {
        self.models.get(model)
    }

    /// The tariff `billing_provider` charges for `model` when it lists one,
    /// else the resolved tariff.
    pub fn rate(&self, model: &str, billing_provider: Option<&str>) -> Option<&ModelCost> {
        billing_provider
            .and_then(|provider| self.billed.get(provider))
            .and_then(|models| models.get(model))
            .or_else(|| self.get(model))
    }

    pub fn contains_key(&self, model: &str) -> bool {
        self.models.contains_key(model)
    }

    pub fn values(&self) -> impl Iterator<Item = &ModelCost> {
        self.models.values()
    }

    /// Set `model`'s tariff for every session, whoever bills it.
    pub fn insert(&mut self, model: String, cost: ModelCost) {
        for models in self.billed.values_mut() {
            models.remove(&model);
        }
        self.models.insert(model, cost);
    }

    /// Shadow this table with `overlay`: its tariffs replace these for every
    /// model it prices, its billing-provider tariffs included.
    fn merge(&mut self, overlay: PricingTable) {
        for (model, cost) in overlay.models {
            self.insert(model, cost);
        }
        for (provider, models) in overlay.billed {
            self.billed.entry(provider).or_default().extend(models);
        }
    }
}

const GITHUB_COPILOT: &str = "github-copilot";

/// Providers whose own tariff bills a session whenever burn reads that
/// session from their harness; see [`billing_provider`].
const BILLING_PROVIDERS: &[&str] = &[GITHUB_COPILOT];

/// The provider that bills every model call of a `source` session, when the
/// harness fixes it. Copilot CLI routes every call through GitHub Copilot.
/// Other harnesses can run against several providers under the same model
/// id, so their turns take the resolved tariff.
pub(crate) fn billing_provider(source: SourceKind) -> Option<&'static str> {
    match source {
        SourceKind::CopilotCli => Some(GITHUB_COPILOT),
        _ => None,
    }
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
    /// `owner/model` naming the lab that publishes the model, e.g.
    /// `moonshotai/kimi-k3` on every reseller's `kimi-k3`.
    #[serde(default)]
    canonical_model_id: Option<String>,
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
    cache_write_1h: Option<f64>,
    #[serde(default)]
    reasoning: Option<f64>,
    #[serde(default)]
    tiers: Vec<ModelsDevCostTier>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevCostTier {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    #[serde(default)]
    cache_write_1h: Option<f64>,
    #[serde(default)]
    reasoning: Option<f64>,
    #[serde(default)]
    tier: ModelsDevTierCondition,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevTierCondition {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: Option<HashMap<String, ModelsDevModel>>,
}

/// Provider order in the file is not a pricing contract: models.dev adds
/// provider blocks anywhere, so [`resolve_tariff`] never consults it.
type ModelsDevRoot = HashMap<String, ModelsDevProvider>;

/// models.dev publishes a single `cache_write` tariff, which for Claude is
/// the 5-minute-TTL write (1.25x input). Claude prompt caching bills
/// 1-hour-TTL writes at 2x the base input tariff (long-context tiers
/// included) on every host that offers the 1-hour TTL — Anthropic, Vertex
/// AI, Bedrock — so Claude entries derive `cache_write_1h` from `input`
/// unless the entry sets `cache_write_1h` explicitly. Other model families
/// have one cache-write tariff regardless of TTL.
const CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Claude ids across hosts: `claude-sonnet-4-6`, `claude-sonnet-4-6@default`
/// (Vertex), `us.anthropic.claude-sonnet-4-6` (Bedrock),
/// `anthropic/claude-sonnet-4.6` (routers).
fn is_claude_model(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("claude")
}

/// Resolve the 1-hour cache-write tariff: explicit field, then the Claude
/// TTL rule, then the single cache-write tariff.
fn cache_write_1h_rate(model_id: &str, explicit: Option<f64>, input: f64, cache_write: f64) -> f64 {
    explicit.unwrap_or(if is_claude_model(model_id) {
        input * CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER
    } else {
        cache_write
    })
}

/// First-party vendors, in precedence order, whose listing of a model id
/// prices it when no entry for that id names an owner that lists it
/// (`canonical_model_id` is missing on older first-party models).
const PRIMARY_PRICING_PROVIDERS: &[&str] =
    &["anthropic", "openai", "google", "google-vertex", "xai"];

/// Bare model IDs owned by a primary pricing provider, plus burn's aliases for
/// those IDs. Unlike the models.dev snapshot, this set is append-only:
/// `pnpm run pricing:update` retains the existing catalog and unions the
/// outgoing and incoming primary-provider IDs before replacing the snapshot.
/// That history prevents a retired first-party model or its logged alias from
/// silently inheriting a reseller tariff after its primary entry disappears.
const BUILTIN_PRIMARY_MODEL_IDS_JSON: &str = include_str!("../../data/primary-model-ids.json");

/// Bundled `models.dev.json` snapshot. Refreshed via `pnpm run pricing:update`,
/// which writes through to the SDK crate's `data/` copy. Vendoring inside the
/// crate is required so `cargo package` / `cargo publish --dry-run` can verify
/// the tarball without network access.
const BUILTIN_PRICING_JSON: &str = include_str!("../../data/models.dev.json");

/// Parsed `BUILTIN_PRICING_JSON`. Parsing the snapshot allocates a
/// `HashMap` of several hundred entries, and `load_builtin_pricing` is on the
/// hot path of multiple SDK verbs that each used to re-parse it.
static BUILTIN_PRICING: LazyLock<PricingTable> = LazyLock::new(|| {
    let protected_models: HashSet<String> = serde_json::from_str(BUILTIN_PRIMARY_MODEL_IDS_JSON)
        .expect("bundled primary-model-ids.json must parse");
    parse_pricing_with_protected_models(BUILTIN_PRICING_JSON, &protected_models)
        .expect("bundled models.dev.json must parse")
});

/// Load the bundled `models.dev` snapshot. No I/O — the JSON is embedded at
/// compile time via `include_str!` and parsed once into a `LazyLock` cache.
pub fn load_builtin_pricing() -> PricingTable {
    BUILTIN_PRICING.clone()
}

/// Load pricing, optionally merging an override file over the builtin. When
/// `override_path` is `Some` and the file exists + parses, its entries shadow
/// builtin entries on key collision, billing-provider tariffs included (see
/// [`PricingTable::merge`]). Any read or parse error on the override silently
/// falls back to the builtin.
pub fn load_pricing(override_path: Option<&Path>) -> PricingTable {
    let mut table = load_builtin_pricing();
    if let Some(path) = override_path {
        if let Ok(user) = load_from_file(path) {
            table.merge(user);
        }
    }
    table
}

fn load_from_file(path: &Path) -> io::Result<PricingTable> {
    let raw = fs::read_to_string(path)?;
    parse_pricing(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub(crate) fn parse_pricing(raw: &str) -> serde_json::Result<PricingTable> {
    parse_pricing_with_protected_models(raw, &HashSet::new())
}

fn parse_pricing_with_protected_models(
    raw: &str,
    protected_models: &HashSet<String>,
) -> serde_json::Result<PricingTable> {
    let parsed: ModelsDevRoot = serde_json::from_str(raw)?;
    Ok(flatten(&parsed, protected_models))
}

/// One provider's priced listing of a model id.
struct Listing<'a> {
    provider: &'a str,
    cost: ModelCost,
}

/// Flatten a nested `provider → model → cost` map into the per-model table
/// burn uses for lookup, one tariff per model id chosen by
/// [`resolve_tariff`]. Entries lacking `input` or `output` are skipped so no
/// half-priced model surfaces.
fn flatten(root: &ModelsDevRoot, protected_models: &HashSet<String>) -> PricingTable {
    let mut table = PricingTable::new();
    let mut listings: HashMap<&str, Vec<Listing<'_>>> = HashMap::new();
    let mut owner_votes: HashMap<&str, HashMap<&str, usize>> = HashMap::new();
    for (provider_id, provider) in root {
        let Some(models) = provider.models.as_ref() else {
            continue;
        };
        for (id, model) in models {
            if let Some((owner, _)) = model
                .canonical_model_id
                .as_deref()
                .and_then(|canonical| canonical.split_once('/'))
            {
                *owner_votes.entry(id).or_default().entry(owner).or_default() += 1;
            }
            let Some(cost) = model_cost(id, model) else {
                continue;
            };
            if BILLING_PROVIDERS.contains(&provider_id.as_str()) && !is_free(&cost) {
                table
                    .billed
                    .entry(provider_id.clone())
                    .or_default()
                    .insert(id.clone(), cost.clone());
            }
            listings.entry(id).or_default().push(Listing {
                provider: provider_id,
                cost,
            });
        }
    }
    for (id, listings) in listings {
        let owners = ranked_owners(owner_votes.remove(id).unwrap_or_default());
        if let Some(cost) = resolve_tariff(&listings, &owners, protected_models.contains(id)) {
            table.models.insert(id.to_string(), cost.clone());
        }
    }
    table
}

/// Owners named by a model id's `canonical_model_id`s, most-named first, ties
/// in name order.
fn ranked_owners(votes: HashMap<&str, usize>) -> Vec<&str> {
    let mut owners: Vec<(&str, usize)> = votes.into_iter().collect();
    owners.sort_by(|(a, a_votes), (b, b_votes)| b_votes.cmp(a_votes).then_with(|| a.cmp(b)));
    owners.into_iter().map(|(owner, _)| owner).collect()
}

/// Choose a model id's tariff from its priced `listings`. A listing free on
/// both input and output is a free tier or plan, not a token tariff, so it
/// takes part only when every listing is free. Of the rest:
///
/// 1. the owner's own listing, trying `owners` in rank order;
/// 2. else the listing of the first [`PRIMARY_PRICING_PROVIDERS`] vendor
///    that has one;
/// 3. else nothing when the id was ever first-party (`protected`), so a
///    retired first-party model is unpriced rather than inheriting a
///    reseller tariff;
/// 4. else the tariff the most listings agree on, ties going to the tariff
///    whose providers include the alphabetically first provider id.
fn resolve_tariff<'a>(
    listings: &'a [Listing<'_>],
    owners: &[&str],
    protected: bool,
) -> Option<&'a ModelCost> {
    let any_paid = listings.iter().any(|listing| !is_free(&listing.cost));
    let tariffs: Vec<&Listing<'_>> = listings
        .iter()
        .filter(|listing| !any_paid || !is_free(&listing.cost))
        .collect();
    let listed_by = |provider: &str| {
        tariffs
            .iter()
            .find(|listing| listing.provider == provider)
            .map(|listing| &listing.cost)
    };
    if let Some(cost) = owners.iter().find_map(|owner| listed_by(owner)) {
        return Some(cost);
    }
    if let Some(cost) = PRIMARY_PRICING_PROVIDERS
        .iter()
        .find_map(|vendor| listed_by(vendor))
    {
        return Some(cost);
    }
    if protected {
        return None;
    }
    consensus_tariff(&tariffs)
}

fn consensus_tariff<'a>(listings: &[&'a Listing<'_>]) -> Option<&'a ModelCost> {
    // (tariff, listings agreeing, alphabetically first provider among them)
    let mut groups: Vec<(&ModelCost, usize, &str)> = Vec::new();
    for listing in listings {
        match groups
            .iter_mut()
            .find(|(cost, _, _)| **cost == listing.cost)
        {
            Some((_, count, first)) => {
                *count += 1;
                *first = (*first).min(listing.provider);
            }
            None => groups.push((&listing.cost, 1, listing.provider)),
        }
    }
    groups
        .into_iter()
        .max_by(|(_, a_count, a_first), (_, b_count, b_first)| {
            a_count.cmp(b_count).then_with(|| b_first.cmp(a_first))
        })
        .map(|(cost, _, _)| cost)
}

/// Free on both input and output: a free tier or plan, not a token tariff.
fn is_free(cost: &ModelCost) -> bool {
    cost.input == 0.0 && cost.output == 0.0
}

/// A listing's tariff, or `None` when it lacks `input` or `output`.
fn model_cost(id: &str, model: &ModelsDevModel) -> Option<ModelCost> {
    let cost = model.cost.as_ref()?;
    let (input, output) = (cost.input?, cost.output?);
    // Models that don't publish a cache-write rate get billed at the input
    // rate for cache creation.
    let cache_write = cost.cache_write.unwrap_or(input);
    Some(ModelCost {
        input,
        output,
        cache_read: cost.cache_read.unwrap_or(0.0),
        cache_write,
        cache_write_1h: cache_write_1h_rate(id, cost.cache_write_1h, input, cache_write),
        reasoning: cost.reasoning,
        reasoning_mode: if cost.reasoning.is_some() {
            ReasoningMode::Separate
        } else {
            ReasoningMode::SameAsOutput
        },
        context_tiers: context_tiers(id, cost),
    })
}

fn context_tiers(model_id: &str, cost: &ModelsDevCost) -> Vec<ModelCostTier> {
    let mut tiers: Vec<ModelCostTier> = cost
        .tiers
        .iter()
        .filter_map(|tier| {
            if tier.tier.kind != "context" {
                return None;
            }
            let context_tokens = tier.tier.size?;
            let input = tier.input?;
            let output = tier.output?;
            let cache_write = tier.cache_write.unwrap_or(input);
            Some(ModelCostTier {
                context_tokens,
                input,
                output,
                cache_read: tier.cache_read.unwrap_or(0.0),
                cache_write,
                cache_write_1h: cache_write_1h_rate(
                    model_id,
                    tier.cache_write_1h,
                    input,
                    cache_write,
                ),
                reasoning: tier.reasoning,
            })
        })
        .collect();
    tiers.sort_by_key(|tier| tier.context_tokens);
    tiers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_snapshot_parses_and_has_anthropic_models() {
        // Presence and shape only: exact tariffs move with upstream
        // repricings, which the refresh workflow already reports.
        let table = load_builtin_pricing();
        for model in ["claude-opus-4-8", "claude-fable-5"] {
            let cost = table
                .get(model)
                .unwrap_or_else(|| panic!("{model} present"));
            assert!(cost.input > 0.0, "{model} has an input tariff");
            assert!(cost.output > 0.0, "{model} has an output tariff");
            assert!(cost.cache_read >= 0.0, "{model} has a cache-read tariff");
            assert!(cost.cache_write > 0.0, "{model} has a cache-write tariff");
        }
        assert!(table.contains_key("claude-opus-4-7"), "opus-4-7 present");
        assert!(
            table.contains_key("claude-sonnet-4-6"),
            "sonnet-4-6 present"
        );
        assert!(table.contains_key("claude-haiku-4-5"), "haiku-4-5 present");
    }

    #[test]
    fn builtin_snapshot_has_gpt_5_5_pricing() {
        let table = load_builtin_pricing();
        let cost = table.get("gpt-5.5").expect("gpt-5.5 present");
        assert!(cost.input > 0.0, "gpt-5.5 has an input tariff");
        assert!(cost.output > 0.0, "gpt-5.5 has an output tariff");
        assert!(cost.cache_read >= 0.0, "gpt-5.5 has a cache-read tariff");
    }

    #[test]
    fn builtin_snapshot_has_current_generation_models() {
        let table = load_builtin_pricing();
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "gpt-5.6-sol",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
        ] {
            assert!(table.contains_key(model), "{model} present");
        }
        // Structural tier assertion only: the dollar tariffs move with
        // upstream repricings, which the refresh workflow already reports.
        let sol = table.get("gpt-5.6-sol").unwrap();
        let tier = sol
            .context_tiers
            .iter()
            .find(|tier| tier.context_tokens == 272_000)
            .expect("gpt-5.6-sol has a 272k context tier");
        assert!(tier.input > 0.0, "tier has an input tariff");
        assert!(tier.output > 0.0, "tier has an output tariff");
    }

    #[test]
    fn builtin_snapshot_does_not_price_retired_primary_models_from_resellers() {
        let table = load_builtin_pricing();
        for model in [
            "claude-sonnet-4-20250514",
            "gemini-2.5-flash-preview-05-20",
            "gpt-5-codex",
        ] {
            assert!(
                !table.contains_key(model),
                "retired first-party model {model} must be unpriced"
            );
        }
    }

    #[test]
    fn builtin_primary_models_are_all_recorded_in_ownership_history() {
        let root: ModelsDevRoot = serde_json::from_str(BUILTIN_PRICING_JSON).unwrap();
        let protected: HashSet<String> =
            serde_json::from_str(BUILTIN_PRIMARY_MODEL_IDS_JSON).unwrap();

        for provider_id in PRIMARY_PRICING_PROVIDERS {
            let Some(models) = root
                .get(*provider_id)
                .and_then(|provider| provider.models.as_ref())
            else {
                continue;
            };
            for model_id in models.keys() {
                assert!(
                    protected.contains(model_id),
                    "primary model {provider_id}/{model_id} is missing from primary-model-ids.json"
                );
            }
        }
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
    fn flatten_derives_claude_1h_cache_write_from_input() {
        let raw = r#"{
            "anthropic": {
                "models": {
                    "claude-example": {
                        "cost": {
                            "input": 3,
                            "output": 15,
                            "cache_read": 0.3,
                            "cache_write": 3.75,
                            "tiers": [{
                                "input": 6,
                                "output": 22.5,
                                "cache_read": 0.6,
                                "cache_write": 7.5,
                                "tier": { "type": "context", "size": 200000 }
                            }]
                        }
                    }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let cost = table.get("claude-example").unwrap();
        assert_eq!(cost.cache_write, 3.75);
        assert_eq!(cost.cache_write_1h, 6.0);
        assert_eq!(cost.context_tiers[0].cache_write, 7.5);
        assert_eq!(cost.context_tiers[0].cache_write_1h, 12.0);
    }

    #[test]
    fn flatten_keeps_single_cache_write_tariff_for_other_models() {
        let raw = r#"{
            "acme": {
                "models": {
                    "acme-v1": {
                        "cost": {
                            "input": 3,
                            "output": 15,
                            "cache_write": 3.75,
                            "tiers": [{
                                "input": 6,
                                "output": 22.5,
                                "cache_write": 7.5,
                                "tier": { "type": "context", "size": 200000 }
                            }]
                        }
                    },
                    "acme-no-cache-write": { "cost": { "input": 2, "output": 8 } }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let cost = table.get("acme-v1").unwrap();
        assert_eq!(cost.cache_write_1h, 3.75);
        assert_eq!(cost.context_tiers[0].cache_write_1h, 7.5);
        assert_eq!(
            table.get("acme-no-cache-write").unwrap().cache_write_1h,
            2.0
        );
    }

    #[test]
    fn flatten_derives_1h_cache_write_for_claude_on_every_host() {
        let raw = r#"{
            "google-vertex": {
                "models": {
                    "claude-sonnet-4-6@default": {
                        "cost": { "input": 3, "output": 15, "cache_write": 3.75 }
                    },
                    "gemini-example": {
                        "cost": { "input": 1.25, "output": 10, "cache_write": 1.5 }
                    }
                }
            },
            "amazon-bedrock": {
                "models": {
                    "us.anthropic.Claude-Fable-5": {
                        "cost": { "input": 10, "output": 50, "cache_write": 12.5 }
                    }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        assert_eq!(
            table
                .get("claude-sonnet-4-6@default")
                .unwrap()
                .cache_write_1h,
            6.0
        );
        assert_eq!(
            table
                .get("us.anthropic.Claude-Fable-5")
                .unwrap()
                .cache_write_1h,
            20.0
        );
        assert_eq!(table.get("gemini-example").unwrap().cache_write_1h, 1.5);
    }

    #[test]
    fn flatten_prefers_explicit_1h_cache_write_tariff() {
        let raw = r#"{
            "anthropic": {
                "models": {
                    "claude-explicit": {
                        "cost": { "input": 3, "output": 15, "cache_write": 3.75, "cache_write_1h": 5 }
                    }
                }
            },
            "acme": {
                "models": {
                    "acme-explicit": {
                        "cost": {
                            "input": 3,
                            "output": 15,
                            "cache_write": 3.75,
                            "cache_write_1h": 4.5,
                            "tiers": [{
                                "input": 6,
                                "output": 22.5,
                                "cache_write_1h": 9,
                                "tier": { "type": "context", "size": 200000 }
                            }]
                        }
                    }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        assert_eq!(table.get("claude-explicit").unwrap().cache_write_1h, 5.0);
        let acme = table.get("acme-explicit").unwrap();
        assert_eq!(acme.cache_write_1h, 4.5);
        assert_eq!(acme.context_tiers[0].cache_write_1h, 9.0);
    }

    #[test]
    fn builtin_snapshot_prices_1h_cache_writes_per_model_family() {
        let table = load_builtin_pricing();
        for model in [
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "us.anthropic.claude-fable-5",
        ] {
            let cost = table.get(model).unwrap();
            assert_eq!(cost.cache_write_1h, cost.input * 2.0, "{model}");
        }
        let gpt = table.get("gpt-5.5").unwrap();
        assert_eq!(gpt.cache_write_1h, gpt.cache_write);
    }

    /// `id`'s tariff as `provider` lists it in the bundled snapshot.
    pub(crate) fn builtin_listing(provider: &str, id: &str) -> ModelCost {
        let root: ModelsDevRoot = serde_json::from_str(BUILTIN_PRICING_JSON).unwrap();
        let model = &root[provider].models.as_ref().unwrap()[id];
        model_cost(id, model).unwrap_or_else(|| panic!("{provider}/{id} is priced"))
    }

    /// The same providers in both file orders.
    fn both_orders(blocks: &[&str]) -> [String; 2] {
        let forward = blocks.join(",");
        let reverse = blocks.iter().rev().copied().collect::<Vec<_>>().join(",");
        [format!("{{{forward}}}"), format!("{{{reverse}}}")]
    }

    #[test]
    fn flatten_prices_a_model_at_its_owners_tariff_in_any_file_order() {
        // The kimi-k3 shape: the owner, a reseller that agrees with it, and
        // a later reseller billing cache reads at the input rate.
        for raw in both_orders(&[
            r#""moonshotai": { "models": { "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3 } } } }"#,
            r#""reseller-a": { "models": { "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 0.3 } } } }"#,
            r#""reseller-b": { "models": { "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 3 } } } }"#,
            r#""reseller-c": { "models": { "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 3 } } } }"#,
        ]) {
            let table = parse_pricing(&raw).unwrap();
            let cost = table.get("kimi-x").unwrap();
            assert_eq!(cost.cache_read, 0.3, "{raw}");
            assert_eq!(cost.cache_write, 3.0, "{raw}");
        }
    }

    #[test]
    fn flatten_tries_owners_most_named_first_then_by_name() {
        let listing = |provider: &str, owner: &str, input: u32| {
            format!(
                r#""{provider}": {{ "models": {{ "m": {{ "canonical_model_id": "{owner}/m",
                    "cost": {{ "input": {input}, "output": 1 }} }} }} }}"#
            )
        };
        // lab-b is named twice, lab-a once: lab-b's own listing wins.
        let named = [
            listing("lab-a", "lab-a", 1),
            listing("lab-b", "lab-b", 2),
            listing("reseller", "lab-b", 3),
        ];
        // lab-a and lab-b tie: the first by name wins.
        let tied = [listing("lab-b", "lab-b", 2), listing("lab-a", "lab-a", 1)];
        // The most-named owner lists nothing, so the next one prices it.
        let absent = [
            listing("reseller-a", "lab-z", 7),
            listing("reseller-b", "lab-z", 7),
            listing("lab-a", "lab-a", 1),
        ];
        for (blocks, input) in [(&named[..], 2.0), (&tied[..], 1.0), (&absent[..], 1.0)] {
            let blocks: Vec<&str> = blocks.iter().map(String::as_str).collect();
            for raw in both_orders(&blocks) {
                assert_eq!(
                    parse_pricing(&raw).unwrap().get("m").unwrap().input,
                    input,
                    "{raw}"
                );
            }
        }
    }

    #[test]
    fn flatten_prefers_primary_vendors_in_precedence_order() {
        for raw in both_orders(&[
            r#""google-vertex": { "models": { "m": { "cost": { "input": 4, "output": 1 } } } }"#,
            r#""google": { "models": { "m": { "cost": { "input": 3, "output": 1 } } } }"#,
            r#""openai": { "models": { "m": { "cost": { "input": 2, "output": 1 } } } }"#,
            r#""xai": { "models": { "m": { "cost": { "input": 5, "output": 1 } } } }"#,
        ]) {
            assert_eq!(
                parse_pricing(&raw).unwrap().get("m").unwrap().input,
                2.0,
                "{raw}"
            );
        }
    }

    #[test]
    fn flatten_prices_a_reseller_only_model_at_the_tariff_most_listings_agree_on() {
        for raw in both_orders(&[
            r#""alpha": { "models": { "m": { "cost": { "input": 9, "output": 9 } } } }"#,
            r#""beta": { "models": { "m": { "cost": { "input": 1, "output": 2 } } } }"#,
            r#""gamma": { "models": { "m": { "cost": { "input": 1, "output": 2 } } } }"#,
        ]) {
            let cost = parse_pricing(&raw).unwrap().get("m").cloned().unwrap();
            assert_eq!((cost.input, cost.output), (1.0, 2.0), "{raw}");
        }
    }

    #[test]
    fn flatten_never_lets_free_listings_outvote_a_paid_tariff() {
        for raw in both_orders(&[
            r#""plan-a": { "models": { "m": { "cost": { "input": 0, "output": 0 } } } }"#,
            r#""plan-b": { "models": { "m": { "cost": { "input": 0, "output": 0 } } } }"#,
            r#""paid": { "models": { "m": { "cost": { "input": 0, "output": 4 } } } }"#,
        ]) {
            assert_eq!(
                parse_pricing(&raw).unwrap().get("m").unwrap().output,
                4.0,
                "{raw}"
            );
        }
        let only_free = r#"{
            "plan-a": { "models": { "m": { "cost": { "input": 0, "output": 0 } } } }
        }"#;
        assert_eq!(
            parse_pricing(only_free).unwrap().get("m").unwrap().output,
            0.0
        );
    }

    #[test]
    fn flatten_never_takes_a_free_owner_or_primary_listing_over_a_paid_one() {
        for (free_block, provider) in [
            (
                r#""lab": { "models": { "m": { "canonical_model_id": "lab/m",
                "cost": { "input": 0, "output": 0 } } } }"#,
                "owner",
            ),
            (
                r#""openai": { "models": { "m": { "cost": { "input": 0, "output": 0 } } } }"#,
                "primary",
            ),
        ] {
            let paid = r#""reseller": { "models": { "m": { "canonical_model_id": "lab/m",
                "cost": { "input": 0.1, "output": 0.2 } } } }"#;
            for raw in both_orders(&[free_block, paid]) {
                let cost = parse_pricing(&raw).unwrap().get("m").cloned().unwrap();
                assert_eq!((cost.input, cost.output), (0.1, 0.2), "{provider}: {raw}");
            }
        }
    }

    #[test]
    fn a_free_billing_provider_listing_never_shadows_the_resolved_tariff() {
        let table = parse_pricing(
            r#"{
                "acme": { "models": { "m": { "cost": { "input": 2, "output": 8 } } } },
                "github-copilot": { "models": { "m": { "cost": { "input": 0, "output": 0 } } } }
            }"#,
        )
        .unwrap();
        assert_eq!(table.rate("m", Some(GITHUB_COPILOT)).unwrap().input, 2.0);
    }

    #[test]
    fn flatten_breaks_reseller_ties_by_provider_id_not_file_order() {
        for raw in both_orders(&[
            r#""zeta": { "models": { "m": { "cost": { "input": 9, "output": 8 } } } }"#,
            r#""beta": { "models": { "m": { "cost": { "input": 5, "output": 5 } } } }"#,
            r#""alpha": { "models": { "m": { "cost": { "input": 1, "output": 2 } } } }"#,
            r#""omega": { "models": { "m": { "cost": { "input": 5, "output": 5 } } } }"#,
            r#""delta": { "models": { "m": { "cost": { "input": 1, "output": 2 } } } }"#,
        ]) {
            // {alpha, delta} and {beta, omega} tie at two listings; alpha
            // sorts first.
            assert_eq!(
                parse_pricing(&raw).unwrap().get("m").unwrap().input,
                1.0,
                "{raw}"
            );
        }
    }

    #[test]
    fn billing_provider_names_copilot_for_copilot_sessions_only() {
        assert_eq!(
            billing_provider(SourceKind::CopilotCli),
            Some("github-copilot")
        );
        for source in [
            SourceKind::ClaudeCode,
            SourceKind::Codex,
            SourceKind::Opencode,
            SourceKind::AnthropicApi,
            SourceKind::OpenaiApi,
            SourceKind::GeminiApi,
        ] {
            assert_eq!(billing_provider(source), None, "{source}");
        }
    }

    const COPILOT_FIXTURE: &str = r#"{
        "moonshotai": { "models": {
            "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 0.3 } },
            "kimi-only-owner": { "cost": { "input": 2, "output": 4 } }
        } },
        "github-copilot": { "models": {
            "kimi-x": { "canonical_model_id": "moonshotai/kimi-x",
                "cost": { "input": 3, "output": 15, "cache_read": 0.5 } },
            "copilot-unpriced": { "cost": { "input": 1 } }
        } },
        "reseller": { "models": {
            "kimi-x": { "cost": { "input": 3, "output": 15, "cache_read": 0.9 } }
        } }
    }"#;

    #[test]
    fn rate_prefers_the_billing_providers_tariff_then_the_resolved_one() {
        let table = parse_pricing(COPILOT_FIXTURE).unwrap();
        let copilot = Some(GITHUB_COPILOT);
        assert_eq!(table.rate("kimi-x", copilot).unwrap().cache_read, 0.5);
        assert_eq!(table.rate("kimi-x", None).unwrap().cache_read, 0.3);
        assert_eq!(
            table.rate("kimi-x", Some("reseller")).unwrap().cache_read,
            0.3
        );
        assert_eq!(table.rate("kimi-only-owner", copilot).unwrap().input, 2.0);
        assert!(table.rate("copilot-unpriced", copilot).is_none());

        let lookup = |source| {
            crate::analyze::cost::lookup_model_rate("kimi-x", Some(source), &table)
                .unwrap()
                .cache_read
        };
        assert_eq!(lookup(SourceKind::CopilotCli), 0.5);
        assert_eq!(lookup(SourceKind::Opencode), 0.3);
    }

    #[test]
    fn insert_sets_a_models_tariff_for_every_billing_provider() {
        let mut table = parse_pricing(COPILOT_FIXTURE).unwrap();
        let mut custom = table.get("kimi-x").unwrap().clone();
        custom.cache_read = 0.01;
        table.insert("kimi-x".into(), custom);
        assert_eq!(
            table
                .rate("kimi-x", Some(GITHUB_COPILOT))
                .unwrap()
                .cache_read,
            0.01
        );
        assert_eq!(table.rate("kimi-x", None).unwrap().cache_read, 0.01);
    }

    #[test]
    fn merge_shadows_resolved_and_billing_provider_tariffs() {
        let mut table = parse_pricing(COPILOT_FIXTURE).unwrap();
        table.merge(
            parse_pricing(
                r#"{
                    "user": { "models": {
                        "kimi-only-owner": { "cost": { "input": 7, "output": 7 } }
                    } },
                    "github-copilot": { "models": {
                        "kimi-x": { "cost": { "input": 3, "output": 15, "cache_read": 0.02 } }
                    } }
                }"#,
            )
            .unwrap(),
        );
        let copilot = Some(GITHUB_COPILOT);
        assert_eq!(table.rate("kimi-x", copilot).unwrap().cache_read, 0.02);
        assert_eq!(table.rate("kimi-only-owner", copilot).unwrap().input, 7.0);
        assert_eq!(table.rate("kimi-only-owner", None).unwrap().input, 7.0);
    }

    #[test]
    fn builtin_snapshot_prices_kimi_k3_at_moonshots_tariff() {
        // Resellers listed after Moonshot bill kimi-k3 cache reads at the
        // input rate; Moonshot's own tariff prices the bare id.
        let table = load_builtin_pricing();
        let moonshot = builtin_listing("moonshotai", "kimi-k3");
        assert_eq!(table.get("kimi-k3"), Some(&moonshot));
        assert!(moonshot.cache_read < moonshot.input);
        assert_eq!(
            table.rate("kimi-k3", Some(GITHUB_COPILOT)),
            Some(&builtin_listing(GITHUB_COPILOT, "kimi-k3"))
        );
    }

    #[test]
    fn flatten_prefers_primary_vendor_over_later_reseller_copy() {
        let raw = r#"{
            "openai": {
                "models": {
                    "gpt-example": { "cost": { "input": 5, "output": 30 } }
                }
            },
            "reseller": {
                "models": {
                    "gpt-example": { "cost": { "input": 1.5, "output": 12, "reasoning": 12 } }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let cost = table.get("gpt-example").unwrap();
        assert_eq!(cost.input, 5.0);
        assert_eq!(cost.output, 30.0);
        assert_eq!(cost.reasoning_mode, ReasoningMode::SameAsOutput);
    }

    #[test]
    fn flatten_prefers_primary_vendor_when_reseller_is_listed_first() {
        let raw = r#"{
            "reseller": {
                "models": {
                    "gpt-example": { "cost": { "input": 1.5, "output": 12, "reasoning": 12 } }
                }
            },
            "openai": {
                "models": {
                    "gpt-example": { "cost": { "input": 5, "output": 30 } }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let cost = table.get("gpt-example").unwrap();
        assert_eq!(cost.input, 5.0);
        assert_eq!(cost.output, 30.0);
        assert_eq!(cost.reasoning_mode, ReasoningMode::SameAsOutput);
    }

    #[test]
    fn flatten_prefers_google_over_later_reseller_copy() {
        let raw = r#"{
            "google": {
                "models": {
                    "gemini-example": { "cost": { "input": 0.5, "output": 60 } }
                }
            },
            "reseller": {
                "models": {
                    "gemini-example": { "cost": { "input": 0.5, "output": 3 } }
                }
            }
        }"#;
        let table = parse_pricing(raw).unwrap();
        let cost = table.get("gemini-example").unwrap();
        assert_eq!(cost.input, 0.5);
        assert_eq!(cost.output, 60.0);
    }

    #[test]
    fn protected_primary_model_never_defaults_cache_fields_from_reseller() {
        let raw = r#"{
            "reseller": {
                "models": {
                    "retired-primary": { "cost": { "input": 2.7, "output": 13.5 } }
                }
            }
        }"#;
        let protected = HashSet::from(["retired-primary".to_string()]);
        let table = parse_pricing_with_protected_models(raw, &protected).unwrap();
        assert!(
            !table.contains_key("retired-primary"),
            "a reseller entry without cache tariffs must not become a ModelCost"
        );
    }

    #[test]
    fn protected_internal_alias_never_takes_a_direct_reseller_price() {
        let raw = r#"{
            "reseller": {
                "models": {
                    "codex-auto-review": { "cost": { "input": 2.7, "output": 13.5 } }
                }
            }
        }"#;
        let protected: HashSet<String> =
            serde_json::from_str(BUILTIN_PRIMARY_MODEL_IDS_JSON).unwrap();
        assert!(protected.contains("codex-auto-review"));
        let table = parse_pricing_with_protected_models(raw, &protected).unwrap();
        assert!(!table.contains_key("codex-auto-review"));
    }

    #[test]
    fn flatten_keeps_reseller_exclusive_model_that_was_never_primary() {
        let raw = r#"{
            "reseller": {
                "models": {
                    "reseller-exclusive": { "cost": { "input": 1.2, "output": 4.8 } }
                }
            }
        }"#;
        let protected = HashSet::from(["some-other-model".to_string()]);
        let table = parse_pricing_with_protected_models(raw, &protected).unwrap();
        let cost = table
            .get("reseller-exclusive")
            .expect("never-primary reseller model remains priced");
        assert_eq!(cost.input, 1.2);
        assert_eq!(cost.output, 4.8);
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

    #[test]
    fn load_pricing_override_can_price_a_retired_primary_model() {
        let override_path = std::env::temp_dir().join(format!(
            "relayburn-retired-pricing-test-{}.json",
            std::process::id()
        ));
        let raw = r#"{
            "user": {
                "models": {
                    "gpt-5-codex": {
                        "cost": {
                            "input": 7,
                            "output": 11,
                            "cache_read": 0.7,
                            "cache_write": 8,
                            "cache_write_1h": 13
                        }
                    }
                }
            }
        }"#;
        fs::write(&override_path, raw).unwrap();
        let table = load_pricing(Some(&override_path));
        let _ = fs::remove_file(&override_path);

        let cost = table
            .get("gpt-5-codex")
            .expect("user override restores retired model pricing");
        assert_eq!(cost.input, 7.0);
        assert_eq!(cost.output, 11.0);
        assert_eq!(cost.cache_read, 0.7);
        assert_eq!(cost.cache_write, 8.0);
        assert_eq!(cost.cache_write_1h, 13.0);
    }
}
