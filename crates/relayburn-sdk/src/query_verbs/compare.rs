use super::*;

// ---------------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareOptions {
    pub models: Vec<String>,
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub workflow: Option<String>,
    pub agent: Option<String>,
    pub provider: Option<Vec<String>>,
    pub min_sample: Option<u64>,
    pub min_fidelity: Option<FidelityClass>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareExcludedBreakdown {
    pub total: u64,
    pub aggregate_only: u64,
    pub cost_only: u64,
    pub partial: u64,
    pub usage_only: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareCellResult {
    pub model: String,
    pub category: String,
    pub turns: u64,
    pub edit_turns: u64,
    pub one_shot_turns: u64,
    pub priced_turns: u64,
    pub total_cost: f64,
    pub cost_per_turn: Option<f64>,
    pub one_shot_rate: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub median_retries: Option<f64>,
    pub no_data: bool,
    pub insufficient_sample: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareModelTotal {
    pub turns: u64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareFidelityBlock {
    pub minimum: FidelityClass,
    pub excluded: CompareExcludedBreakdown,
    pub summary: FidelitySummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    pub analyzed_turns: u64,
    pub min_sample: u64,
    pub models: Vec<String>,
    pub categories: Vec<String>,
    pub totals: BTreeMap<String, CompareModelTotal>,
    pub cells: Vec<CompareCellResult>,
    pub fidelity: CompareFidelityBlock,
}

impl LedgerHandle {
    pub fn compare(&self, opts: CompareOptions) -> Result<CompareResult> {
        if opts.models.len() < 2 {
            anyhow::bail!("compare: needs at least 2 models");
        }

        let min_fidelity = opts.min_fidelity.unwrap_or(FidelityClass::UsageOnly);
        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
            None,
        )?;
        let mut enrichment = BTreeMap::new();
        if let Some(workflow) = opts.workflow {
            enrichment.insert("workflowId".to_string(), workflow);
        }
        if let Some(agent) = opts.agent {
            enrichment.insert("agentId".to_string(), agent);
        }
        if !enrichment.is_empty() {
            q.enrichment = Some(enrichment);
        }

        // Mirror TS `compare()` (`packages/sdk/index.js`):
        //   - provider filter (drops turns whose derived provider is excluded)
        //   - fidelity summary over the *post-provider*, *pre-fidelity-gate*
        //     slice (the TS path calls `summarizeFidelity(turns)` here)
        //   - fidelity-gate filter (a no-op when minimum is `partial`)
        //   - `analyzedTurns = filteredTurns.length` — i.e. AFTER the
        //     fidelity gate but BEFORE the model allow-list, which is
        //     applied inside `build_compare_table`.
        //
        // Crucially: do NOT pre-filter `turns` by `opts.models`. The TS
        // contract is that `analyzedTurns` and `fidelity.summary` describe
        // the slice the comparison was *drawn from*, not the cells. The
        // model allow-list is honored by `build_compare_table` via
        // `opts.models`, which also pre-seeds requested models that
        // produced zero turns as all-empty columns.
        let mut turns = self.inner.query_turns(&q)?;
        if let Some(filter) = normalize_provider_filter(opts.provider) {
            turns.retain(|t| {
                let provider = crate::analyze::provider_for(&t.turn).provider;
                filter.contains(&provider.to_ascii_lowercase())
            });
        }

        let fidelity_summary =
            summarize_fidelity_from_iter(turns.iter().map(|t| t.turn.fidelity.as_ref()));
        if min_fidelity != FidelityClass::Partial {
            turns.retain(|t| has_minimum_fidelity(t.turn.fidelity.as_ref(), min_fidelity));
        }

        let pricing = load_pricing(None);
        let table = build_compare_table(
            &turns,
            &AnalyzeCompareOptions {
                pricing: &pricing,
                models: Some(opts.models),
                min_sample: opts.min_sample,
            },
        );
        Ok(shape_compare_result(
            table,
            turns.len() as u64,
            min_fidelity,
            fidelity_summary,
        ))
    }

    /// Time-bucketed [`compare`]: the same model comparison computed per
    /// `bucket_secs`-wide window across the `--since` range (turn-level
    /// partition; a clean per-turn fold). Note: small buckets thin each
    /// model×category cell's sample, so `insufficientSample` trips far more
    /// often than over the whole window.
    pub fn compare_timeseries(
        &self,
        opts: CompareOptions,
        bucket_secs: u64,
    ) -> Result<CompareTimeseries> {
        if opts.models.len() < 2 {
            anyhow::bail!("compare: needs at least 2 models");
        }
        let min_fidelity = opts.min_fidelity.unwrap_or(FidelityClass::UsageOnly);
        let models = opts.models.clone();
        let min_sample = opts.min_sample;

        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
            None,
        )?;
        let mut enrichment = BTreeMap::new();
        if let Some(workflow) = opts.workflow {
            enrichment.insert("workflowId".to_string(), workflow);
        }
        if let Some(agent) = opts.agent {
            enrichment.insert("agentId".to_string(), agent);
        }
        if !enrichment.is_empty() {
            q.enrichment = Some(enrichment);
        }

        let mut turns = self.inner.query_turns(&q)?;
        if let Some(filter) = normalize_provider_filter(opts.provider) {
            turns.retain(|t| {
                let provider = crate::analyze::provider_for(&t.turn).provider;
                filter.contains(&provider.to_ascii_lowercase())
            });
        }
        let pricing = load_pricing(None);

        let Some((buckets, per_bucket)) = super::partition_into_buckets(
            turns,
            q.since.as_deref(),
            q.until.as_deref(),
            bucket_secs,
            |t| &t.turn.ts,
        )?
        else {
            return Ok(CompareTimeseries {
                bucket_secs,
                buckets: Vec::new(),
            });
        };

        let out = per_bucket
            .into_iter()
            .enumerate()
            .map(|(i, mut bturns)| {
                let fidelity_summary =
                    summarize_fidelity_from_iter(bturns.iter().map(|t| t.turn.fidelity.as_ref()));
                if min_fidelity != FidelityClass::Partial {
                    bturns.retain(|t| has_minimum_fidelity(t.turn.fidelity.as_ref(), min_fidelity));
                }
                let table = build_compare_table(
                    &bturns,
                    &AnalyzeCompareOptions {
                        pricing: &pricing,
                        models: Some(models.clone()),
                        min_sample,
                    },
                );
                let result = shape_compare_result(
                    table,
                    bturns.len() as u64,
                    min_fidelity,
                    fidelity_summary,
                );
                CompareBucket {
                    start: buckets.start_iso(i),
                    end: buckets.end_iso(i),
                    result,
                }
            })
            .collect();

        Ok(CompareTimeseries {
            bucket_secs,
            buckets: out,
        })
    }
}

/// One time bucket of a [`CompareTimeseries`] — the [`CompareResult`] for turns
/// in `[start, end)`, flattened alongside the window bounds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareBucket {
    pub start: String,
    pub end: String,
    #[serde(flatten)]
    pub result: CompareResult,
}

/// A time-series of model comparisons, one [`CompareBucket`] per window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareTimeseries {
    #[serde(rename = "bucketSeconds")]
    pub bucket_secs: u64,
    pub buckets: Vec<CompareBucket>,
}

pub fn compare(opts: CompareOptions) -> Result<CompareResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.compare(CompareOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn compute_compare_excluded(
    summary: &FidelitySummary,
    minimum: FidelityClass,
) -> CompareExcludedBreakdown {
    let mut out = CompareExcludedBreakdown {
        total: 0,
        aggregate_only: 0,
        cost_only: 0,
        partial: 0,
        usage_only: 0,
    };
    if minimum == FidelityClass::Partial {
        return out;
    }

    for class in [
        FidelityClass::CostOnly,
        FidelityClass::AggregateOnly,
        FidelityClass::Partial,
        FidelityClass::UsageOnly,
        FidelityClass::Full,
    ] {
        if fidelity_rank(class) >= fidelity_rank(minimum) {
            continue;
        }
        let n = *summary.by_class.get(&class).unwrap_or(&0);
        if n == 0 {
            continue;
        }
        out.total += n;
        match class {
            FidelityClass::AggregateOnly => out.aggregate_only += n,
            FidelityClass::CostOnly => out.cost_only += n,
            FidelityClass::Partial => out.partial += n,
            FidelityClass::UsageOnly => out.usage_only += n,
            FidelityClass::Full => {}
        }
    }
    out
}

fn shape_compare_result(
    table: CompareTable,
    analyzed_turns: u64,
    minimum: FidelityClass,
    summary: FidelitySummary,
) -> CompareResult {
    let mut totals = BTreeMap::new();
    for (model, total) in &table.totals {
        totals.insert(
            model.clone(),
            CompareModelTotal {
                turns: total.turns,
                total_cost: total.total_cost,
            },
        );
    }

    let mut cells = Vec::new();
    for model in &table.models {
        let Some(row) = table.cells.get(model) else {
            continue;
        };
        for category in &table.categories {
            let Some(cell) = row.get(category) else {
                continue;
            };
            cells.push(CompareCellResult {
                model: model.clone(),
                category: category.clone(),
                turns: cell.turns,
                edit_turns: cell.edit_turns,
                one_shot_turns: cell.one_shot_turns,
                priced_turns: cell.priced_turns,
                total_cost: round_digits(cell.total_cost, 6),
                cost_per_turn: cell.cost_per_turn.map(|n| round_digits(n, 6)),
                one_shot_rate: cell.one_shot_rate.map(|n| round_digits(n, 4)),
                cache_hit_rate: cell.cache_hit_rate.map(|n| round_digits(n, 4)),
                median_retries: cell.median_retries,
                no_data: cell.no_data,
                insufficient_sample: cell.insufficient_sample,
            });
        }
    }

    let excluded = compute_compare_excluded(&summary, minimum);
    CompareResult {
        analyzed_turns,
        min_sample: table.min_sample,
        models: table.models,
        categories: table.categories,
        totals,
        cells,
        fidelity: CompareFidelityBlock {
            minimum,
            excluded,
            summary,
        },
    }
}

fn fidelity_rank(class: FidelityClass) -> u8 {
    match class {
        FidelityClass::CostOnly => 0,
        FidelityClass::AggregateOnly => 1,
        FidelityClass::Partial => 2,
        FidelityClass::UsageOnly => 3,
        FidelityClass::Full => 4,
    }
}

/// Round to `digits` decimal places matching JS `Number(n.toFixed(digits))` /
/// Rust `format!("{n:.digits$}")` semantics (round half-to-even on the decimal
/// string), rather than `f64::round`'s half-away-from-zero. The presenter
/// layer re-formats these values with `format!("{:.N}")`, so rounding the same
/// way here keeps that second pass idempotent — at exact ties the two
/// rounding modes otherwise disagree in the last digit.
fn round_digits(n: f64, digits: i32) -> f64 {
    let s = format!("{n:.*}", digits.max(0) as usize);
    s.parse().unwrap_or(n)
}
