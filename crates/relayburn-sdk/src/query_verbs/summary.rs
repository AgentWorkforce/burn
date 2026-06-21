use super::*;

// ---------------------------------------------------------------------------
// summary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Enrichment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_tag: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryToolRow {
    pub tool: String,
    pub tokens: u64,
    pub cost: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryModelRow {
    pub model: String,
    pub tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTagRow {
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub tokens: u64,
    pub cost: f64,
    pub turn_count: u64,
}

/// Per-outcome turn counts, surfaced by `burn summary` for the one-line
/// outcome breakdown (`142 end_turn, 3 max_tokens, 1 refusal, 0 pause`).
///
/// Counts mirror the [`StopReason`] enum variants plus a `none` slot for
/// turns whose row carried no `stop_reason` field at all — that's Codex
/// today (no field in the rollout schema) and any pre-3.0 ledger row that
/// was ingested before the reader started populating the enum.
///
/// `Silent` is reserved for "row exists, carries a stop_reason that we
/// don't recognize" — distinct from `none` so we can spot a future harness
/// regression rather than silently lumping it with Codex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReasonCounts {
    pub end_turn: u64,
    pub max_tokens: u64,
    pub pause_turn: u64,
    pub stop_sequence: u64,
    pub tool_use: u64,
    pub refusal: u64,
    pub silent: u64,
    /// Turns whose record carried no `stop_reason` field — e.g. Codex
    /// rollouts (the harness doesn't report one) or pre-3.0 ledger rows
    /// from before the reader started parsing the field.
    pub none: u64,
}

impl StopReasonCounts {
    /// Accumulate one turn's outcome into the bucket counts. `None` lands
    /// in [`Self::none`]; unrecognized variants would already be normalized
    /// to [`StopReason::Silent`] upstream by the lenient deserializer.
    pub fn bump(&mut self, reason: Option<StopReason>) {
        match reason {
            None => self.none += 1,
            Some(StopReason::EndTurn) => self.end_turn += 1,
            Some(StopReason::MaxTokens) => self.max_tokens += 1,
            Some(StopReason::PauseTurn) => self.pause_turn += 1,
            Some(StopReason::StopSequence) => self.stop_sequence += 1,
            Some(StopReason::ToolUse) => self.tool_use += 1,
            Some(StopReason::Refusal) => self.refusal += 1,
            Some(StopReason::Silent) => self.silent += 1,
        }
    }

    /// Fold every turn's `stop_reason` into a fresh counts struct.
    pub fn from_turns(turns: &[TurnRecord]) -> Self {
        let mut out = Self::default();
        for t in turns {
            out.bump(t.stop_reason);
        }
        out
    }

    /// True iff every counter is zero — useful for "skip the outcome line
    /// entirely" presentation logic in summary.
    pub fn is_empty(&self) -> bool {
        self.end_turn
            | self.max_tokens
            | self.pause_turn
            | self.stop_sequence
            | self.tool_use
            | self.refusal
            | self.silent
            | self.none
            == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub turn_count: u64,
    pub by_tool: Vec<SummaryToolRow>,
    pub by_model: Vec<SummaryModelRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_tag: Option<Vec<SummaryTagRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_savings: Option<ReplacementSavingsSummary>,
    /// Per-outcome breakdown — `end_turn` / `max_tokens` / `refusal` / etc.
    /// Counts roll up the trailing `stop_reason` of every assistant turn
    /// in the filtered slice. See #437.
    pub stop_reasons: StopReasonCounts,
    /// Count of turns whose model had no entry in the pricing snapshot.
    /// Their cost is reported as $0. Zero when all models are priced.
    #[serde(default)]
    pub unpriced_turns: u64,
    /// Distinct model names (first-seen order) that had no pricing entry.
    /// Empty when all models are priced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

impl LedgerHandle {
    pub fn summary(&self, opts: SummaryOptions) -> Result<Summary> {
        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        if let Some(tags) = opts.tags.clone() {
            validate_tags(&tags)?;
            if !tags.is_empty() {
                q.enrichment = Some(tags);
            }
        }
        let group_by_tag = opts.group_by_tag.clone();
        if let Some(tag) = group_by_tag.as_deref() {
            validate_tag_key(tag, "groupByTag")?;
        }
        let enriched = self.inner.query_turns(&q)?;
        let turns: Vec<TurnRecord> = enriched.iter().map(|e| e.turn.clone()).collect();
        let pricing = load_pricing(None);
        let mut summary = compute_summary(&turns, &pricing);
        if let Some(tag) = group_by_tag {
            summary.by_tag = Some(compute_summary_by_tag(&enriched, &tag, &pricing));
        }
        Ok(summary)
    }
}

pub fn summary(opts: SummaryOptions) -> Result<Summary> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary(SummaryOptions {
        ledger_home: None,
        ..opts
    })
}

fn validate_tags(tags: &Enrichment) -> Result<()> {
    for key in tags.keys() {
        validate_tag_key(key, "tag")?;
    }
    Ok(())
}

fn validate_tag_key(key: &str, label: &str) -> Result<()> {
    if key.is_empty() {
        anyhow::bail!("{label} key must be non-empty");
    }
    Ok(())
}

pub(crate) fn compute_summary(turns: &[TurnRecord], pricing: &PricingTable) -> Summary {
    // First-seen iteration order matches TS `Map` semantics.
    let mut by_tool_order: Vec<String> = Vec::new();
    let mut by_tool: HashMap<String, SummaryToolRow> = HashMap::new();
    let mut by_model_order: Vec<String> = Vec::new();
    let mut by_model: HashMap<String, SummaryModelRow> = HashMap::new();
    let mut total_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;

    for t in turns {
        let cost = cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0);
        let tokens = t.usage.input
            + t.usage.output
            + t.usage.reasoning
            + t.usage.cache_read
            + t.usage.cache_create_5m
            + t.usage.cache_create_1h;
        total_tokens += tokens;
        total_cost += cost;

        let model_row = by_model.entry(t.model.clone()).or_insert_with(|| {
            by_model_order.push(t.model.clone());
            SummaryModelRow {
                model: t.model.clone(),
                tokens: 0,
                cost: 0.0,
            }
        });
        model_row.tokens += tokens;
        model_row.cost += cost;

        for call in &t.tool_calls {
            let tool_row = by_tool.entry(call.name.clone()).or_insert_with(|| {
                by_tool_order.push(call.name.clone());
                SummaryToolRow {
                    tool: call.name.clone(),
                    tokens: 0,
                    cost: 0.0,
                    count: 0,
                }
            });
            tool_row.tokens += tokens;
            tool_row.cost += cost;
            tool_row.count += 1;
        }
    }

    let savings = summarize_replacement_savings(turns, None);
    let replacement_savings = if savings.calls > 0 {
        Some(savings)
    } else {
        None
    };

    // Use the same pricing table that was used for cost accumulation so the
    // count precisely matches which turns contributed $0 to `total_cost`.
    let (unpriced_turns, unpriced_models) = tally_unpriced(turns, pricing);

    Summary {
        total_tokens,
        total_cost,
        turn_count: turns.len() as u64,
        by_tool: by_tool_order
            .into_iter()
            .map(|k| by_tool.remove(&k).unwrap())
            .collect(),
        by_model: by_model_order
            .into_iter()
            .map(|k| by_model.remove(&k).unwrap())
            .collect(),
        by_tag: None,
        replacement_savings,
        stop_reasons: StopReasonCounts::from_turns(turns),
        unpriced_turns,
        unpriced_models,
    }
}

fn compute_summary_by_tag(
    enriched: &[EnrichedTurn],
    tag: &str,
    pricing: &PricingTable,
) -> Vec<SummaryTagRow> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut rows: HashMap<Option<String>, SummaryTagRow> = HashMap::new();

    for e in enriched {
        let value = e.enrichment.get(tag).cloned();
        let tokens = total_tokens_for_turn(&e.turn);
        let cost = cost_for_turn(&e.turn, pricing)
            .map(|c| c.total)
            .unwrap_or(0.0);
        let row = rows.entry(value.clone()).or_insert_with(|| {
            order.push(value.clone());
            SummaryTagRow {
                tag: tag.to_string(),
                value,
                tokens: 0,
                cost: 0.0,
                turn_count: 0,
            }
        });
        row.tokens += tokens;
        row.cost += cost;
        row.turn_count += 1;
    }

    let mut out: Vec<SummaryTagRow> = order
        .into_iter()
        .map(|k| rows.remove(&k).unwrap())
        .collect();
    out.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn total_tokens_for_turn(t: &TurnRecord) -> u64 {
    t.usage.input
        + t.usage.output
        + t.usage.reasoning
        + t.usage.cache_read
        + t.usage.cache_create_5m
        + t.usage.cache_create_1h
}

// ---------------------------------------------------------------------------
// richer summary report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Enrichment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_tag: Option<String>,
    pub agent: Option<String>,
    /// Provider labels to keep. Values are trimmed and matched
    /// case-insensitively against the SDK's effective provider resolver.
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub mode: SummaryReportMode,
    #[serde(default)]
    pub include_quality: bool,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum SummaryReportMode {
    Grouped {
        #[serde(default)]
        by_provider: bool,
    },
    ByTool,
    BySubagentType,
    ByRelationship {
        #[serde(default)]
        subagent: bool,
    },
    SubagentTree {
        #[serde(default)]
        session_id: Option<String>,
    },
}

impl Default for SummaryReportMode {
    fn default() -> Self {
        Self::Grouped { by_provider: false }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SummaryGroupBy {
    Model,
    Provider,
    Tag,
}

impl SummaryGroupBy {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Tag => "tag",
        }
    }

    pub fn json_key(self) -> &'static str {
        match self {
            Self::Model => "byModel",
            Self::Provider => "byProvider",
            Self::Tag => "byTag",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum SummaryReport {
    Grouped(SummaryGroupedReport),
    ByTool(SummaryByToolReport),
    BySubagentType(SummarySubagentTypeReport),
    Relationship(SummaryRelationshipReport),
    SubagentTree(SummarySubagentTreeReport),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryGroupedReport {
    pub group_by: SummaryGroupBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tag_values: Vec<Option<String>>,
    pub turn_count: u64,
    pub rows: Vec<UsageCostAggregateRow>,
    pub total_cost: CostBreakdown,
    pub fidelity: FidelitySummary,
    /// Stable TS-compatible JSON shape for per-cell coverage. Kept in the SDK
    /// so presenters don't rebuild order-sensitive HashMap projections.
    pub per_cell_fidelity: serde_json::Value,
    pub replacement_savings: ReplacementSavingsSummary,
    /// Per-outcome turn counts (issue #437). Always populated; presenters
    /// decide whether to render the line based on `is_empty()`.
    pub stop_reasons: StopReasonCounts,
    /// Paired / orphan subagent transcript counts (issue #435). Populated
    /// by a lazy walk over the Claude `~/.claude/projects/` tree at
    /// summary time — when no sidecars exist anywhere reachable the
    /// `read_dir` short-circuits and the field stays at
    /// `SubagentCounts::default()`. Presenters render the
    /// `subagents: X paired, Y orphan` line only when
    /// `!subagents.is_empty()`.
    #[serde(
        default,
        skip_serializing_if = "crate::reader::SubagentCounts::is_empty"
    )]
    pub subagents: crate::reader::SubagentCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityResult>,
    /// Count of turns whose model had no entry in the pricing snapshot.
    /// Their cost is reported as $0. Zero when all models are priced.
    #[serde(default)]
    pub unpriced_turns: u64,
    /// Distinct model names (first-seen order) that had no pricing entry.
    /// Empty when all models are priced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryToolAttributionMethod {
    Unattributed,
    Sized,
    EvenSplit,
}

impl SummaryToolAttributionMethod {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Unattributed => "unattributed",
            Self::Sized => "sized",
            Self::EvenSplit => "even-split",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryToolAttributionRow {
    pub tool: String,
    pub calls: u64,
    pub attributed_cost: f64,
    pub attribution_method: SummaryToolAttributionMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savings: Option<ToolSavingsAggregate>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryByToolReport {
    pub turn_count: u64,
    pub rows: Vec<SummaryToolAttributionRow>,
    pub unattributed_cost: f64,
    pub fidelity: FidelitySummary,
    pub replacement_savings: ReplacementSavingsSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarySubagentTypeReport {
    pub stats: Vec<SubagentTypeStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipReport {
    pub relationships: Vec<SummaryRelationshipStats>,
    pub subagent_types: Vec<SummaryRelationshipSubagentStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipStats {
    pub relationship_type: RelationshipType,
    pub count: u64,
    pub session_count: u64,
    pub turn_count: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipSubagentStats {
    pub subagent_type: String,
    pub invocations: u64,
    pub turns: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarySubagentTreeReport {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<SubagentTreeNode>,
}

/// One time bucket of a [`SummaryTimeseries`]: the grouped summary totals for
/// turns whose `ts` falls in `[start, end)`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryBucket {
    pub start: String,
    pub end: String,
    pub turn_count: u64,
    pub total_tokens: u64,
    pub total_cost: CostBreakdown,
    pub group_by: SummaryGroupBy,
    pub rows: Vec<UsageCostAggregateRow>,
}

/// A time-series of grouped summary totals — one [`SummaryBucket`] per
/// `bucket_secs`-wide window across the `--since` range. Produced by
/// [`LedgerHandle::summary_timeseries`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTimeseries {
    #[serde(rename = "bucketSeconds")]
    pub bucket_secs: u64,
    pub buckets: Vec<SummaryBucket>,
}

impl LedgerHandle {
    /// Time-bucketed cost/usage totals (the `--bucket` form of the default
    /// grouped summary). Fetches the `--since` window once, then partitions the
    /// turns by `ts` into `bucket_secs`-wide buckets and aggregates each — a
    /// pure per-turn fold, so per-bucket totals sum back to the un-bucketed
    /// total. Supported only for the default grouped (`byModel`/`byProvider`)
    /// summary; the tool/subagent/relationship attribution modes are rejected.
    pub fn summary_timeseries(
        &self,
        opts: SummaryReportOptions,
        bucket_secs: u64,
    ) -> Result<SummaryTimeseries> {
        let by_provider = match &opts.mode {
            SummaryReportMode::Grouped { by_provider } => *by_provider,
            _ => anyhow::bail!(
                "--bucket is only supported with the default grouped summary, not \
                 --by-tool/--by-subagent-type/--by-relationship/--subagent-tree"
            ),
        };
        if opts.group_by_tag.is_some() {
            anyhow::bail!("--bucket is not supported with --group-by-tag");
        }
        if opts.include_quality {
            anyhow::bail!("--bucket is not supported with --quality metrics yet");
        }

        let q = build_summary_report_query(&opts)?;
        let provider_filter = normalize_summary_provider_filter(opts.providers.as_deref());
        let pricing = load_pricing(None);
        let agent_session_ids = match opts.agent.as_deref() {
            Some(agent_id) => Some(resolve_summary_agent_session_tree(&self.inner, agent_id)?),
            None => None,
        };

        let enriched = self.inner.query_turns(&q)?;
        let enriched = filter_summary_enriched_turns(
            enriched,
            opts.agent.as_deref(),
            agent_session_ids.as_ref(),
            provider_filter.as_ref(),
        );
        let turns = summary_turns_from_enriched(&enriched);

        let Some(anchor) = super::bucket_anchor_secs(
            q.since.as_deref(),
            turns
                .iter()
                .filter_map(|t| super::iso_z_to_epoch_secs(&t.ts)),
        ) else {
            return Ok(SummaryTimeseries {
                bucket_secs,
                buckets: Vec::new(),
            });
        };
        let now = super::system_now_secs() as i64;
        super::ensure_bucket_span(anchor, now, bucket_secs)?;
        let buckets = super::Buckets::new(anchor, now, bucket_secs);
        let n = buckets.len();

        let mut per_bucket: Vec<Vec<TurnRecord>> = (0..n).map(|_| Vec::new()).collect();
        for t in turns {
            let Some(ep) = super::iso_z_to_epoch_secs(&t.ts) else {
                continue;
            };
            if let Some(i) = buckets.index_for(ep) {
                per_bucket[i].push(t);
            }
        }

        let group_by = if by_provider {
            SummaryGroupBy::Provider
        } else {
            SummaryGroupBy::Model
        };
        let out = per_bucket
            .into_iter()
            .enumerate()
            .map(|(i, bturns)| {
                let rows = if by_provider {
                    aggregate_by_provider(&bturns, AggregateByProviderOptions::new(&pricing))
                        .into_iter()
                        .map(summary_provider_to_aggregate_row)
                        .collect::<Vec<_>>()
                } else {
                    summary_aggregate_by_model(&bturns, &pricing)
                };
                let total_cost = sum_costs(rows.iter().map(|r| &r.cost));
                let total_tokens: u64 = bturns.iter().map(total_tokens_for_turn).sum();
                SummaryBucket {
                    start: buckets.start_iso(i),
                    end: buckets.end_iso(i),
                    turn_count: bturns.len() as u64,
                    total_tokens,
                    total_cost,
                    group_by,
                    rows,
                }
            })
            .collect();

        Ok(SummaryTimeseries {
            bucket_secs,
            buckets: out,
        })
    }

    pub fn summary_report(&self, opts: SummaryReportOptions) -> Result<SummaryReport> {
        let q = build_summary_report_query(&opts)?;
        let provider_filter = normalize_summary_provider_filter(opts.providers.as_deref());
        let pricing = load_pricing(None);
        let agent_session_ids = match opts.agent.as_deref() {
            Some(agent_id) => Some(resolve_summary_agent_session_tree(&self.inner, agent_id)?),
            None => None,
        };

        if let SummaryReportMode::SubagentTree { session_id } = &opts.mode {
            let session_id = session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| q.session_id.clone())
                .ok_or_else(|| anyhow::anyhow!("subagent tree summary requires a session id"))?;
            let relationships =
                collect_summary_subagent_tree_relationships(&self.inner, &session_id, &q)?;
            let enriched =
                load_summary_subagent_tree_turns(&self.inner, &session_id, &relationships, &q)?;
            let enriched = filter_summary_enriched_turns(
                enriched,
                opts.agent.as_deref(),
                agent_session_ids.as_ref(),
                provider_filter.as_ref(),
            );
            let turns = summary_turns_from_enriched(&enriched);
            let tree_opts =
                BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
            let trees = build_subagent_tree(&turns, &tree_opts);
            let root = trees
                .get(&session_id)
                .cloned()
                .or_else(|| find_summary_tree_node(trees.values(), &session_id));
            return Ok(SummaryReport::SubagentTree(SummarySubagentTreeReport {
                session_id,
                root,
            }));
        }

        let enriched = self.inner.query_turns(&q)?;
        let enriched = filter_summary_enriched_turns(
            enriched,
            opts.agent.as_deref(),
            agent_session_ids.as_ref(),
            provider_filter.as_ref(),
        );
        let turns = summary_turns_from_enriched(&enriched);

        match opts.mode {
            SummaryReportMode::Grouped { by_provider } => {
                let (group_by, tag_key, tag_values, rows) = if let Some(tag_key) =
                    opts.group_by_tag.as_deref()
                {
                    let (rows, values) = summary_aggregate_by_tag(&enriched, tag_key, &pricing);
                    (SummaryGroupBy::Tag, Some(tag_key.to_string()), values, rows)
                } else if by_provider {
                    (
                        SummaryGroupBy::Provider,
                        None,
                        Vec::new(),
                        aggregate_by_provider(&turns, AggregateByProviderOptions::new(&pricing))
                            .into_iter()
                            .map(summary_provider_to_aggregate_row)
                            .collect(),
                    )
                } else {
                    (
                        SummaryGroupBy::Model,
                        None,
                        Vec::new(),
                        summary_aggregate_by_model(&turns, &pricing),
                    )
                };
                let total_cost = sum_costs(rows.iter().map(|r| &r.cost));
                let fidelity = summarize_fidelity(&turns);
                let per_cell_fidelity = summary_per_cell_fidelity_to_value(&rows, group_by);
                let replacement_savings = summarize_replacement_savings(&turns, None);
                let quality = if opts.include_quality {
                    Some(compute_summary_quality_for_turns(&self.inner, &turns)?)
                } else {
                    None
                };
                let stop_reasons = StopReasonCounts::from_turns(&turns);
                // Lazy walk over `~/.claude/projects/` (or the configured
                // override) for the `subagents: X paired, Y orphan`
                // summary line (issue #435). The walk short-circuits when
                // the projects root is missing or every session lacks a
                // `subagents/` subdir — i.e. zero cost on the vast
                // majority of summaries that don't hit a session with
                // sidecar transcripts.
                //
                // When the summary itself is scoped (any of `--session`,
                // `--project`, `--since`, `--workflow`, `--tags`,
                // `--agent`, `--providers`) we restrict the sidecar
                // walk to the same session-id set the rest of the
                // summary covers; otherwise the line could report
                // paired/orphan counts from sessions the user excluded.
                // Un-filtered runs keep the original global walk
                // behavior.
                let session_filter = summary_subagent_session_filter(&opts, &turns);
                let subagents = compute_summary_subagent_counts(session_filter.as_ref());
                let (unpriced_turns, unpriced_models) = tally_unpriced(&turns, &pricing);
                Ok(SummaryReport::Grouped(SummaryGroupedReport {
                    group_by,
                    tag_key,
                    tag_values,
                    turn_count: turns.len() as u64,
                    rows,
                    total_cost,
                    fidelity,
                    per_cell_fidelity,
                    replacement_savings,
                    stop_reasons,
                    subagents,
                    quality,
                    unpriced_turns,
                    unpriced_models,
                }))
            }
            SummaryReportMode::ByTool => {
                let attribution_turns =
                    load_summary_by_tool_attribution_turns(&self.inner, &enriched, &q)?;
                let report = compute_summary_by_tool_report(
                    &self.inner,
                    &turns,
                    &attribution_turns,
                    &pricing,
                )?;
                Ok(SummaryReport::ByTool(report))
            }
            SummaryReportMode::BySubagentType => {
                let stats =
                    aggregate_subagent_type_stats(&turns, &BuildSubagentTreeOptions::new(&pricing));
                Ok(SummaryReport::BySubagentType(SummarySubagentTypeReport {
                    stats,
                }))
            }
            SummaryReportMode::ByRelationship { subagent } => {
                let relationships = self
                    .inner
                    .query_relationships(&summary_relationship_query_for_turn_slice(&q))?;
                let matches =
                    match_summary_relationships_to_turns(&relationships, &turns, &pricing);
                let stats = aggregate_summary_relationship_stats(&matches);
                if subagent {
                    let subagent_types = aggregate_summary_relationship_subagent_stats(&matches);
                    let relationships = stats
                        .into_iter()
                        .filter(|s| s.relationship_type == RelationshipType::Subagent)
                        .collect();
                    Ok(SummaryReport::Relationship(SummaryRelationshipReport {
                        relationships,
                        subagent_types,
                    }))
                } else {
                    Ok(SummaryReport::Relationship(SummaryRelationshipReport {
                        relationships: stats,
                        subagent_types: Vec::new(),
                    }))
                }
            }
            SummaryReportMode::SubagentTree { .. } => unreachable!(),
        }
    }
}

pub fn summary_report(opts: SummaryReportOptions) -> Result<SummaryReport> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary_report(SummaryReportOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn summary_fidelity_summary_to_value(s: &FidelitySummary) -> serde_json::Value {
    let mut by_class = serde_json::Map::new();
    for class in [
        FidelityClass::Full,
        FidelityClass::UsageOnly,
        FidelityClass::AggregateOnly,
        FidelityClass::CostOnly,
        FidelityClass::Partial,
    ] {
        by_class.insert(
            class.wire_str().to_string(),
            serde_json::json!(*s.by_class.get(&class).unwrap_or(&0)),
        );
    }

    let mut by_granularity = serde_json::Map::new();
    for g in [
        UsageGranularity::PerTurn,
        UsageGranularity::PerMessage,
        UsageGranularity::PerSessionAggregate,
        UsageGranularity::CostOnly,
    ] {
        by_granularity.insert(
            g.wire_str().to_string(),
            serde_json::json!(*s.by_granularity.get(&g).unwrap_or(&0)),
        );
    }

    let mut missing = serde_json::Map::new();
    for field in [
        "hasInputTokens",
        "hasOutputTokens",
        "hasReasoningTokens",
        "hasCacheReadTokens",
        "hasCacheCreateTokens",
        "hasToolCalls",
        "hasToolResultEvents",
        "hasSessionRelationships",
        "hasRawContent",
    ] {
        missing.insert(
            field.to_string(),
            serde_json::json!(*s.missing_coverage.get(field).unwrap_or(&0)),
        );
    }

    let mut out = serde_json::Map::new();
    out.insert("total".into(), serde_json::json!(s.total));
    out.insert("byClass".into(), serde_json::Value::Object(by_class));
    out.insert(
        "byGranularity".into(),
        serde_json::Value::Object(by_granularity),
    );
    out.insert("missingCoverage".into(), serde_json::Value::Object(missing));
    out.insert("unknown".into(), serde_json::json!(s.unknown));
    serde_json::Value::Object(out)
}

pub fn summary_per_cell_fidelity_to_value(
    rows: &[UsageCostAggregateRow],
    group_by: SummaryGroupBy,
) -> serde_json::Value {
    let cells: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let fields = [
                ("input", &r.coverage.input),
                ("output", &r.coverage.output),
                ("reasoning", &r.coverage.reasoning),
                ("cacheRead", &r.coverage.cache_read),
                ("cacheCreate", &r.coverage.cache_create),
            ];
            let mut fields_map = serde_json::Map::new();
            let mut partial = false;
            for (name, c) in fields {
                if summary_cell_is_partial(c) || (c.known == 0 && c.missing > 0) {
                    partial = true;
                }
                fields_map.insert(
                    name.to_string(),
                    serde_json::json!({
                        "known": c.known,
                        "missing": c.missing,
                    }),
                );
            }
            serde_json::json!({
                "label": r.label,
                "partial": partial,
                "fields": serde_json::Value::Object(fields_map),
            })
        })
        .collect();
    serde_json::json!({
        "groupBy": group_by.wire_str(),
        "cells": cells,
    })
}

pub fn summary_replacement_savings_to_value(
    savings: &ReplacementSavingsSummary,
) -> serde_json::Value {
    let mut by_tool: Vec<serde_json::Value> = savings
        .by_tool
        .iter()
        .map(|(name, agg)| {
            serde_json::json!({
                "tool": name,
                "calls": agg.calls,
                "collapsedCalls": agg.collapsed_calls,
                "estimatedTokensSaved": agg.estimated_tokens_saved,
            })
        })
        .collect();
    by_tool.sort_by(|a, b| {
        let av = a
            .get("estimatedTokensSaved")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let bv = b
            .get("estimatedTokensSaved")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        bv.cmp(&av).then_with(|| {
            let at = a
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let bt = b
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            at.cmp(bt)
        })
    });
    serde_json::json!({
        "calls": savings.calls,
        "collapsedCalls": savings.collapsed_calls,
        "estimatedTokensSaved": savings.estimated_tokens_saved,
        "byTool": by_tool,
    })
}

fn build_summary_report_query(opts: &SummaryReportOptions) -> Result<Query> {
    let mut q = build_query(
        opts.session.as_deref(),
        opts.project.as_deref(),
        opts.since.as_deref(),
    )?;
    if let Some(tag) = opts.group_by_tag.as_deref() {
        validate_tag_key(tag, "groupByTag")?;
    }
    let mut enrichment = BTreeMap::new();
    if let Some(workflow) = &opts.workflow {
        enrichment.insert("workflowId".to_string(), workflow.clone());
    }
    if let Some(tags) = opts.tags.as_ref() {
        validate_tags(tags)?;
        for (key, value) in tags {
            if let Some(existing) = enrichment.get(key) {
                if existing != value {
                    anyhow::bail!(
                        "conflicting filters for tag \"{key}\" ({existing:?} vs {value:?})"
                    );
                }
            }
            enrichment.insert(key.clone(), value.clone());
        }
    }
    if !enrichment.is_empty() {
        q.enrichment = Some(enrichment);
    }
    Ok(q)
}

fn normalize_summary_provider_filter(providers: Option<&[String]>) -> Option<ProviderFilter> {
    let providers: ProviderFilter = providers
        .unwrap_or(&[])
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if providers.is_empty() {
        None
    } else {
        Some(providers)
    }
}

fn filter_summary_enriched_turns(
    turns: Vec<EnrichedTurn>,
    agent_id: Option<&str>,
    agent_session_ids: Option<&HashSet<String>>,
    provider_filter: Option<&ProviderFilter>,
) -> Vec<EnrichedTurn> {
    turns
        .into_iter()
        .filter(|t| summary_agent_passes(t, agent_id, agent_session_ids))
        .filter(|t| summary_provider_passes(&t.turn, provider_filter))
        .collect()
}

fn summary_agent_passes(
    t: &EnrichedTurn,
    agent_id: Option<&str>,
    session_ids: Option<&HashSet<String>>,
) -> bool {
    let Some(agent_id) = agent_id else {
        return true;
    };
    if t.enrichment.get("agentId").map(String::as_str) == Some(agent_id) {
        return true;
    }
    if t.enrichment.get("parentAgentId").map(String::as_str) == Some(agent_id) {
        return true;
    }
    session_ids
        .map(|ids| ids.contains(&t.turn.session_id))
        .unwrap_or(false)
}

fn summary_provider_passes(t: &TurnRecord, provider_filter: Option<&ProviderFilter>) -> bool {
    let Some(filter) = provider_filter else {
        return true;
    };
    let provider = provider_for(t).provider.to_ascii_lowercase();
    filter.contains(&provider)
}

fn summary_turns_from_enriched(enriched: &[EnrichedTurn]) -> Vec<TurnRecord> {
    enriched.iter().map(|e| e.turn.clone()).collect()
}

fn load_summary_by_tool_attribution_turns(
    ledger: &crate::ledger::Ledger,
    selected: &[EnrichedTurn],
    q: &Query,
) -> Result<Vec<TurnRecord>> {
    let session_ids: Vec<String> = selected
        .iter()
        .map(|e| e.turn.session_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let turns = ledger.query_turns_in_sessions(
        &Query {
            source: q.source,
            ..Default::default()
        },
        &session_ids,
    )?;
    let mut by_key: IndexMap<String, EnrichedTurn> = IndexMap::new();
    for t in turns {
        let key = format!(
            "{}|{}|{}",
            t.turn.source.wire_str(),
            t.turn.session_id,
            t.turn.message_id,
        );
        by_key.insert(key, t);
    }
    Ok(by_key.into_values().map(|e| e.turn).collect())
}

fn resolve_summary_agent_session_tree(
    ledger: &crate::ledger::Ledger,
    agent_id: &str,
) -> Result<HashSet<String>> {
    Ok(collect_summary_agent_session_tree(
        &ledger.query_relationships(&Query::default())?,
        agent_id,
    ))
}

pub(crate) fn collect_summary_agent_session_tree(
    relationships: &[SessionRelationshipRecord],
    agent_id: &str,
) -> HashSet<String> {
    let mut by_parent: HashMap<String, Vec<&SessionRelationshipRecord>> = HashMap::new();
    for r in relationships {
        if r.relationship_type != RelationshipType::Subagent {
            continue;
        }
        let Some(parent) = r.related_session_id.as_deref() else {
            continue;
        };
        if parent.is_empty() {
            continue;
        }
        by_parent.entry(parent.to_string()).or_default().push(r);
    }

    let mut sessions = HashSet::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([agent_id.to_string()]);
    while let Some(parent) = queue.pop_front() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        for child in by_parent.get(&parent).into_iter().flatten() {
            sessions.insert(child.session_id.clone());
            queue.push_back(child.session_id.clone());
            if let Some(agent) = child.agent_id.as_ref() {
                if !agent.is_empty() {
                    queue.push_back(agent.clone());
                }
            }
        }
    }
    sessions
}

/// Resolve the Claude projects root and run [`count_subagents_under`]
/// against it for the `subagents: X paired, Y orphan` summary line.
///
/// We honor `BURN_CLAUDE_PROJECTS_DIR` so tests (and integration
/// fixtures) can point at a sandbox without scanning the developer's
/// `~/.claude`. The env var also lets the CLI summary remain
/// reproducible against a fixture-only test suite. When unset we fall
/// back to `$HOME/.claude/projects`; if that doesn't exist the
/// underlying walk returns `(0, 0)` and the summary line is skipped.
///
/// `session_filter` matches the rest of the summary's filter set:
/// `None` means "no filter — count every session reachable from the
/// projects root" (the un-filtered `burn summary` path); `Some(set)`
/// means "only count sidecars whose session id is in `set`" so a
/// `burn summary --session A` / `--project B` / `--since 24h` run gets
/// a subagent count scoped to the same sessions the rest of the
/// numbers cover.
fn compute_summary_subagent_counts(
    session_filter: Option<&HashSet<String>>,
) -> crate::reader::SubagentCounts {
    use crate::reader::count_subagents_under;
    let root = if let Some(p) = std::env::var_os("BURN_CLAUDE_PROJECTS_DIR") {
        std::path::PathBuf::from(p)
    } else {
        // Defaults to `~/.claude/projects` (HOME, then USERPROFILE on
        // Windows — see crate::util::home_dir) when
        // `BURN_CLAUDE_PROJECTS_DIR` is unset.
        crate::util::home_dir().join(".claude").join("projects")
    };
    count_subagents_under(&root, session_filter)
}

/// Build the session-id filter set the subagent counter should descend
/// into. Returns `None` when `opts` carries no scoping filters, which
/// preserves the original "scan every reachable session" behavior for
/// the bare `burn summary` invocation. Returns `Some(set)` when any
/// filter (`session`, `project`, `since`, `workflow`, `tags`, `agent`,
/// `providers`) is active — `set` is the session ids that survived
/// every filter, derived from the already-filtered `turns` slice.
///
/// Plumbing the filter via the filtered turn set (instead of e.g.
/// duplicating the SQL filters inside the walker) ensures the count
/// can never diverge from the rest of the summary numbers: anything
/// that drops a session from the row aggregates also drops it from the
/// subagent count.
pub(crate) fn summary_subagent_session_filter(
    opts: &SummaryReportOptions,
    turns: &[TurnRecord],
) -> Option<HashSet<String>> {
    let has_filter = opts.session.is_some()
        || opts.project.is_some()
        || opts.since.is_some()
        || opts.workflow.is_some()
        || opts.agent.is_some()
        || opts.tags.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
        || opts
            .providers
            .as_ref()
            .map(|p| !p.is_empty())
            .unwrap_or(false);
    if !has_filter {
        return None;
    }
    Some(turns.iter().map(|t| t.session_id.clone()).collect())
}

fn compute_summary_quality_for_turns(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<QualityResult> {
    let content_by_session = load_summary_content_for_quality(ledger, turns)?;
    Ok(compute_quality(
        turns,
        &ComputeQualityOptions {
            content_by_session: Some(&content_by_session),
            now_ms: None,
        },
    ))
}

fn load_summary_content_for_quality(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<HashMap<String, Vec<ContentRecord>>> {
    let mut seen = HashSet::new();
    let mut out = HashMap::new();
    for t in turns {
        if !seen.insert(t.session_id.clone()) {
            continue;
        }
        let records = ledger.query_content(&Query {
            session_id: Some(t.session_id.clone()),
            ..Default::default()
        })?;
        if !records.is_empty() {
            out.insert(t.session_id.clone(), records);
        }
    }
    Ok(out)
}

fn summary_aggregate_by_tag(
    enriched: &[EnrichedTurn],
    tag_key: &str,
    pricing: &PricingTable,
) -> (Vec<UsageCostAggregateRow>, Vec<Option<String>>) {
    let mut by_value: HashMap<Option<String>, UsageCostAggregateRow> = HashMap::new();
    let mut order: Vec<Option<String>> = Vec::new();
    for enriched in enriched {
        let value = enriched.enrichment.get(tag_key).cloned();
        let label = value.clone().unwrap_or_else(|| "(untagged)".to_string());
        let row = by_value.entry(value.clone()).or_insert_with(|| {
            order.push(value.clone());
            summary_empty_row(&label)
        });
        row.turns += 1;
        row.usage.input += enriched.turn.usage.input;
        row.usage.output += enriched.turn.usage.output;
        row.usage.reasoning += enriched.turn.usage.reasoning;
        row.usage.cache_read += enriched.turn.usage.cache_read;
        row.usage.cache_create_5m += enriched.turn.usage.cache_create_5m;
        row.usage.cache_create_1h += enriched.turn.usage.cache_create_1h;
        summary_accumulate_coverage(
            &mut row.coverage,
            enriched.turn.fidelity.as_ref().map(|f| &f.coverage),
        );
        if let Some(c) = cost_for_turn(&enriched.turn, pricing) {
            row.cost.total += c.total;
            row.cost.input += c.input;
            row.cost.output += c.output;
            row.cost.reasoning += c.reasoning;
            row.cost.cache_read += c.cache_read;
            row.cost.cache_create += c.cache_create;
        }
    }

    let mut pairs: Vec<(Option<String>, UsageCostAggregateRow)> = order
        .into_iter()
        .map(|value| {
            let row = by_value.remove(&value).unwrap();
            (value, row)
        })
        .collect();
    pairs.sort_by(|a, b| {
        b.1.cost
            .total
            .partial_cmp(&a.1.cost.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let (values, rows): (Vec<Option<String>>, Vec<UsageCostAggregateRow>) =
        pairs.into_iter().unzip();
    (rows, values)
}

fn summary_aggregate_by_model(
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Vec<UsageCostAggregateRow> {
    let mut by_model: IndexMap<String, UsageCostAggregateRow> = IndexMap::new();
    for t in turns {
        let key = if t.model.is_empty() {
            "unknown".to_string()
        } else {
            t.model.clone()
        };
        let row = by_model
            .entry(key.clone())
            .or_insert_with(|| summary_empty_row(&key));
        row.turns += 1;
        row.usage.input += t.usage.input;
        row.usage.output += t.usage.output;
        row.usage.reasoning += t.usage.reasoning;
        row.usage.cache_read += t.usage.cache_read;
        row.usage.cache_create_5m += t.usage.cache_create_5m;
        row.usage.cache_create_1h += t.usage.cache_create_1h;
        summary_accumulate_coverage(&mut row.coverage, t.fidelity.as_ref().map(|f| &f.coverage));
        if let Some(c) = cost_for_turn(t, pricing) {
            row.cost.total += c.total;
            row.cost.input += c.input;
            row.cost.output += c.output;
            row.cost.reasoning += c.reasoning;
            row.cost.cache_read += c.cache_read;
            row.cost.cache_create += c.cache_create;
        }
    }
    let mut rows: Vec<UsageCostAggregateRow> = by_model.into_values().collect();
    rows.sort_by(|a, b| {
        b.cost
            .total
            .partial_cmp(&a.cost.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn summary_provider_to_aggregate_row(p: ProviderAggregateRow) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: p.label,
        turns: p.turns,
        usage: p.usage,
        cost: p.cost,
        coverage: p.coverage,
    }
}

fn summary_empty_row(label: &str) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: label.to_string(),
        turns: 0,
        usage: Usage::default(),
        cost: CostBreakdown {
            model: label.to_string().into(),
            total: 0.0,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_create: 0.0,
        },
        coverage: RowCoverage::default(),
    }
}

fn summary_accumulate_coverage(target: &mut RowCoverage, coverage: Option<&Coverage>) {
    for f in [
        CoverageField::Input,
        CoverageField::Output,
        CoverageField::Reasoning,
        CoverageField::CacheRead,
        CoverageField::CacheCreate,
    ] {
        let known = match coverage {
            None => true,
            Some(c) => match f {
                CoverageField::Input => c.has_input_tokens,
                CoverageField::Output => c.has_output_tokens,
                CoverageField::Reasoning => c.has_reasoning_tokens,
                CoverageField::CacheRead => c.has_cache_read_tokens,
                CoverageField::CacheCreate => c.has_cache_create_tokens,
            },
        };
        let slot = target.field_mut(f);
        if known {
            slot.known += 1;
        } else {
            slot.missing += 1;
        }
    }
}

fn summary_cell_is_partial(c: &FieldCoverage) -> bool {
    c.known > 0 && c.missing > 0
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SummaryToolAgg {
    pub(crate) calls: u64,
    pub(crate) cost: f64,
    pub(crate) sized_cost: f64,
    pub(crate) even_split_cost: f64,
}

#[derive(Debug, Default)]
struct SummaryUserTurnSizeBucket {
    tool_bytes_by_id: HashMap<String, u64>,
    total_bytes: u64,
}

fn compute_summary_by_tool_report(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
    attribution_turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Result<SummaryByToolReport> {
    let user_turns_by_session = load_summary_user_turns_for_by_tool(ledger, attribution_turns)?;
    let selected_turns = selected_summary_turn_keys(turns);
    let (by_tool, unattributed_cost) = attribute_summary_cost_to_tools(
        attribution_turns,
        pricing,
        &user_turns_by_session,
        Some(&selected_turns),
    );
    let fidelity = summarize_fidelity(turns);
    let replacement_savings = summarize_replacement_savings(turns, None);
    let mut sorted: Vec<(String, SummaryToolAgg)> = by_tool.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let rows = sorted
        .into_iter()
        .map(|(tool, agg)| SummaryToolAttributionRow {
            savings: replacement_savings.by_tool.get(&tool).cloned(),
            tool,
            calls: agg.calls,
            attributed_cost: agg.cost,
            attribution_method: summary_tool_attribution_method(&agg),
        })
        .collect();
    Ok(SummaryByToolReport {
        turn_count: turns.len() as u64,
        rows,
        unattributed_cost,
        fidelity,
        replacement_savings,
    })
}

fn load_summary_user_turns_for_by_tool(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<HashMap<String, Vec<UserTurnRecord>>> {
    let session_ids: BTreeSet<String> = turns.iter().map(|t| t.session_id.clone()).collect();
    let mut out = HashMap::new();
    for session_id in session_ids {
        let rows = ledger.query_user_turns(&Query {
            session_id: Some(session_id.clone()),
            ..Default::default()
        })?;
        if !rows.is_empty() {
            out.insert(session_id, rows);
        }
    }
    Ok(out)
}

fn selected_summary_turn_keys(turns: &[TurnRecord]) -> HashSet<String> {
    turns.iter().map(summary_turn_identity_key).collect()
}

pub(crate) fn attribute_summary_cost_to_tools(
    turns: &[TurnRecord],
    pricing: &PricingTable,
    user_turns_by_session: &HashMap<String, Vec<UserTurnRecord>>,
    selected_turns: Option<&HashSet<String>>,
) -> (IndexMap<String, SummaryToolAgg>, f64) {
    let mut by_tool: IndexMap<String, SummaryToolAgg> = IndexMap::new();
    let mut unattributed = 0.0;
    let mut by_session: IndexMap<String, Vec<&TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session.entry(t.session_id.clone()).or_default().push(t);
    }

    for (session_id, mut list) in by_session {
        list.sort_by_key(|t| t.turn_index);
        let user_turn_size_index = index_summary_user_turn_block_sizes(
            user_turns_by_session
                .get(&session_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        for i in 0..list.len() {
            let turn = list[i];
            if !summary_turn_is_selected(turn, selected_turns) {
                continue;
            }
            let Some(c) = cost_for_turn(turn, pricing) else {
                continue;
            };
            let ingest_cost = c.input + c.cache_read + c.cache_create;

            if i == 0 {
                unattributed += ingest_cost;
                continue;
            }
            let prior = list[i - 1];
            if prior.tool_calls.is_empty() {
                unattributed += ingest_cost;
                continue;
            }

            let key = summary_bridge_key(&prior.message_id, &turn.message_id);
            let sizes = user_turn_size_index.get(&key);
            let sized_bytes: u64 = match sizes {
                Some(s) => prior
                    .tool_calls
                    .iter()
                    .map(|tc| *s.tool_bytes_by_id.get(&tc.id).unwrap_or(&0))
                    .sum(),
                None => 0,
            };
            if let Some(sizes) = sizes.filter(|_| sized_bytes > 0) {
                let allocatable_cost = if sizes.total_bytes > 0 {
                    ingest_cost * (sized_bytes as f64 / sizes.total_bytes as f64).min(1.0)
                } else {
                    ingest_cost
                };
                unattributed += ingest_cost - allocatable_cost;
                let mut raw_shares: Vec<(String, f64)> = Vec::new();
                for tc in &prior.tool_calls {
                    let bytes = *sizes.tool_bytes_by_id.get(&tc.id).unwrap_or(&0);
                    if bytes == 0 {
                        continue;
                    }
                    by_tool.entry(tc.name.clone()).or_default().calls += 1;
                    raw_shares.push((
                        tc.name.clone(),
                        (bytes as f64 / sized_bytes as f64) * allocatable_cost,
                    ));
                }
                let raw_subtotal: f64 = raw_shares.iter().map(|(_, cost)| *cost).sum();
                let scale = if raw_subtotal > allocatable_cost && raw_subtotal > 0.0 {
                    allocatable_cost / raw_subtotal
                } else {
                    1.0
                };
                for (tool, cost) in raw_shares {
                    let share = cost * scale;
                    let agg = by_tool.entry(tool).or_default();
                    agg.cost += share;
                    agg.sized_cost += share;
                }
            } else {
                let share = ingest_cost / prior.tool_calls.len() as f64;
                for tc in &prior.tool_calls {
                    let agg = by_tool.entry(tc.name.clone()).or_default();
                    agg.calls += 1;
                    agg.cost += share;
                    agg.even_split_cost += share;
                }
            }
        }
    }

    (by_tool, unattributed)
}

fn summary_turn_is_selected(turn: &TurnRecord, selected_turns: Option<&HashSet<String>>) -> bool {
    selected_turns
        .map(|keys| keys.contains(&summary_turn_identity_key(turn)))
        .unwrap_or(true)
}

pub(crate) fn summary_turn_identity_key(turn: &TurnRecord) -> String {
    format!(
        "{}\0{}\0{}",
        turn.source.wire_str(),
        turn.session_id,
        turn.message_id
    )
}

fn index_summary_user_turn_block_sizes(
    user_turns: &[UserTurnRecord],
) -> HashMap<String, SummaryUserTurnSizeBucket> {
    let mut out: HashMap<String, SummaryUserTurnSizeBucket> = HashMap::new();
    for user_turn in user_turns {
        let (Some(preceding), Some(following)) = (
            user_turn.preceding_message_id.as_ref(),
            user_turn.following_message_id.as_ref(),
        ) else {
            continue;
        };
        let bucket = out
            .entry(summary_bridge_key(preceding, following))
            .or_default();
        for block in &user_turn.blocks {
            let bytes = block.byte_len;
            bucket.total_bytes += bytes;
            if block.kind != UserTurnBlockKind::ToolResult {
                continue;
            }
            let Some(tool_use_id) = block.tool_use_id.as_ref() else {
                continue;
            };
            *bucket
                .tool_bytes_by_id
                .entry(tool_use_id.clone())
                .or_default() += bytes;
        }
    }
    out
}

fn summary_bridge_key(preceding_message_id: &str, following_message_id: &str) -> String {
    format!("{preceding_message_id}\0{following_message_id}")
}

pub(crate) fn summary_tool_attribution_method(
    agg: &SummaryToolAgg,
) -> SummaryToolAttributionMethod {
    if agg.sized_cost == 0.0 && agg.even_split_cost == 0.0 {
        SummaryToolAttributionMethod::Unattributed
    } else if agg.sized_cost >= agg.even_split_cost {
        SummaryToolAttributionMethod::Sized
    } else {
        SummaryToolAttributionMethod::EvenSplit
    }
}

const SUMMARY_RELATIONSHIP_ORDER: [RelationshipType; 4] = [
    RelationshipType::Root,
    RelationshipType::Continuation,
    RelationshipType::Fork,
    RelationshipType::Subagent,
];

#[derive(Debug, Clone)]
pub(crate) struct SummaryRelationshipMatch {
    pub(crate) relationship_type: RelationshipType,
    pub(crate) session_id: String,
    pub(crate) subagent_type: Option<String>,
    pub(crate) turn_count: u64,
    pub(crate) cost: f64,
}

struct SummaryRelationshipTurnIndex<'a> {
    all_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    main_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    sidechain_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    subagent_by_session_agent: HashMap<String, Vec<&'a TurnRecord>>,
}

fn summary_relationship_query_for_turn_slice(q: &Query) -> Query {
    Query {
        session_id: q.session_id.clone(),
        source: q.source,
        ..Default::default()
    }
}

fn match_summary_relationships_to_turns(
    relationships: &[SessionRelationshipRecord],
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Vec<SummaryRelationshipMatch> {
    let index = build_summary_relationship_turn_index(turns);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in relationships {
        let key = summary_relationship_instance_key(r);
        if !seen.insert(key) {
            continue;
        }
        let matched_turns = summary_turns_for_relationship(r, &index);
        if matched_turns.is_empty() {
            continue;
        }
        let cost = matched_turns
            .iter()
            .map(|t| cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0))
            .sum();
        out.push(SummaryRelationshipMatch {
            relationship_type: r.relationship_type,
            session_id: r.session_id.clone(),
            subagent_type: summary_relationship_subagent_type(r, &matched_turns),
            turn_count: matched_turns.len() as u64,
            cost,
        });
    }
    out
}

fn build_summary_relationship_turn_index(turns: &[TurnRecord]) -> SummaryRelationshipTurnIndex<'_> {
    let mut index = SummaryRelationshipTurnIndex {
        all_by_session: HashMap::new(),
        main_by_session: HashMap::new(),
        sidechain_by_session: HashMap::new(),
        subagent_by_session_agent: HashMap::new(),
    };
    for turn in turns {
        index
            .all_by_session
            .entry(turn.session_id.clone())
            .or_default()
            .push(turn);
        if summary_is_main_thread_turn(turn) {
            index
                .main_by_session
                .entry(turn.session_id.clone())
                .or_default()
                .push(turn);
        }
        if turn
            .subagent
            .as_ref()
            .map(|s| s.is_sidechain)
            .unwrap_or(false)
        {
            index
                .sidechain_by_session
                .entry(turn.session_id.clone())
                .or_default()
                .push(turn);
        }
        if let Some(agent_id) = turn.subagent.as_ref().and_then(|s| s.agent_id.as_ref()) {
            if !agent_id.is_empty() {
                index
                    .subagent_by_session_agent
                    .entry(summary_session_agent_key(&turn.session_id, agent_id))
                    .or_default()
                    .push(turn);
            }
        }
    }
    index
}

fn summary_turns_for_relationship<'a>(
    r: &SessionRelationshipRecord,
    index: &'a SummaryRelationshipTurnIndex<'a>,
) -> Vec<&'a TurnRecord> {
    match r.relationship_type {
        RelationshipType::Root => index
            .main_by_session
            .get(&r.session_id)
            .cloned()
            .unwrap_or_default(),
        RelationshipType::Subagent => {
            if let Some(agent_id) = r.agent_id.as_ref().filter(|s| !s.is_empty()) {
                let key = summary_session_agent_key(&r.session_id, agent_id);
                if let Some(direct) = index.subagent_by_session_agent.get(&key) {
                    if !direct.is_empty() {
                        return direct.clone();
                    }
                }
                if r.session_id == *agent_id {
                    return index
                        .all_by_session
                        .get(&r.session_id)
                        .cloned()
                        .unwrap_or_default();
                }
            }
            if let Some(sidechain) = index.sidechain_by_session.get(&r.session_id) {
                if !sidechain.is_empty() {
                    return sidechain.clone();
                }
            }
            if r.source.wire_str() == "spawn-env" {
                return index
                    .all_by_session
                    .get(&r.session_id)
                    .cloned()
                    .unwrap_or_default();
            }
            Vec::new()
        }
        RelationshipType::Continuation | RelationshipType::Fork => index
            .all_by_session
            .get(&r.session_id)
            .cloned()
            .unwrap_or_default(),
    }
}

pub(crate) fn aggregate_summary_relationship_stats(
    matches: &[SummaryRelationshipMatch],
) -> Vec<SummaryRelationshipStats> {
    #[derive(Default)]
    struct RelationshipSessionRollup {
        relationship_count: u64,
        turn_count: u64,
        cost: f64,
    }

    let mut by_type: HashMap<RelationshipType, HashMap<String, RelationshipSessionRollup>> =
        HashMap::new();
    for m in matches {
        let by_session = by_type.entry(m.relationship_type).or_default();
        let current = by_session.entry(m.session_id.clone()).or_default();
        current.relationship_count += 1;
        current.turn_count += m.turn_count;
        current.cost += m.cost;
    }

    let mut out = Vec::new();
    for relationship_type in SUMMARY_RELATIONSHIP_ORDER {
        let Some(by_session) = by_type.get(&relationship_type) else {
            continue;
        };
        if by_session.is_empty() {
            continue;
        }
        let mut costs: Vec<f64> = by_session.values().map(|rollup| rollup.cost).collect();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let total_cost: f64 = costs.iter().sum();
        let session_count = by_session.len() as u64;
        out.push(SummaryRelationshipStats {
            relationship_type,
            count: by_session
                .values()
                .map(|rollup| rollup.relationship_count)
                .sum(),
            session_count,
            turn_count: by_session.values().map(|rollup| rollup.turn_count).sum(),
            total_cost,
            median_cost: summary_percentile(&costs, 0.5),
            p95_cost: summary_percentile(&costs, 0.95),
            mean_cost: if session_count > 0 {
                total_cost / session_count as f64
            } else {
                0.0
            },
        });
    }
    out
}

fn aggregate_summary_relationship_subagent_stats(
    matches: &[SummaryRelationshipMatch],
) -> Vec<SummaryRelationshipSubagentStats> {
    struct Agg {
        turns: u64,
        total: f64,
        costs: Vec<f64>,
    }
    let mut by_type: IndexMap<String, Agg> = IndexMap::new();
    for m in matches {
        if m.relationship_type != RelationshipType::Subagent {
            continue;
        }
        let ty = m
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        let agg = by_type.entry(ty).or_insert_with(|| Agg {
            turns: 0,
            total: 0.0,
            costs: Vec::new(),
        });
        agg.turns += m.turn_count;
        agg.total += m.cost;
        agg.costs.push(m.cost);
    }

    let mut out = Vec::new();
    for (subagent_type, mut agg) in by_type {
        agg.costs
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let invocations = agg.costs.len() as u64;
        out.push(SummaryRelationshipSubagentStats {
            subagent_type,
            invocations,
            turns: agg.turns,
            total_cost: agg.total,
            median_cost: summary_percentile(&agg.costs, 0.5),
            p95_cost: summary_percentile(&agg.costs, 0.95),
            mean_cost: if invocations > 0 {
                agg.total / invocations as f64
            } else {
                0.0
            },
        });
    }
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn summary_relationship_subagent_type(
    relationship: &SessionRelationshipRecord,
    turns: &[&TurnRecord],
) -> Option<String> {
    if let Some(st) = &relationship.subagent_type {
        return Some(st.clone());
    }
    turns.iter().find_map(|t| {
        t.subagent
            .as_ref()
            .and_then(|s| s.subagent_type.as_ref())
            .cloned()
    })
}

fn summary_relationship_instance_key(r: &SessionRelationshipRecord) -> String {
    [
        r.source.wire_str(),
        r.relationship_type.wire_str(),
        &r.session_id,
        r.related_session_id.as_deref().unwrap_or(""),
        r.agent_id.as_deref().unwrap_or(""),
        r.parent_tool_use_id.as_deref().unwrap_or(""),
    ]
    .join("\0")
}

fn summary_session_agent_key(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}\0{agent_id}")
}

fn summary_is_main_thread_turn(turn: &TurnRecord) -> bool {
    match &turn.subagent {
        None => true,
        Some(sub) => !sub.is_sidechain || sub.agent_id.as_deref() == Some(&turn.session_id),
    }
}

fn summary_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank =
        ((p * sorted.len() as f64).ceil() as i64 - 1).clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[rank]
}

fn collect_summary_subagent_tree_relationships(
    ledger: &crate::ledger::Ledger,
    session_id: &str,
    q: &Query,
) -> Result<Vec<SessionRelationshipRecord>> {
    let relationships = ledger.query_relationships(&Query {
        source: q.source,
        ..Default::default()
    })?;
    Ok(collect_summary_connected_relationships(
        &relationships,
        session_id,
    ))
}

pub(crate) fn collect_summary_connected_relationships(
    relationships: &[SessionRelationshipRecord],
    session_id: &str,
) -> Vec<SessionRelationshipRecord> {
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, r) in relationships.iter().enumerate() {
        for id in summary_relationship_connected_ids(r) {
            if !id.is_empty() {
                by_id.entry(id).or_default().push(idx);
            }
        }
    }

    let mut out: IndexMap<String, SessionRelationshipRecord> = IndexMap::new();
    let mut seen_ids = HashSet::new();
    let mut queue = VecDeque::from([session_id.to_string()]);
    while let Some(id) = queue.pop_front() {
        if !seen_ids.insert(id.clone()) {
            continue;
        }
        let Some(rows) = by_id.get(&id) else {
            continue;
        };
        for idx in rows {
            let r = &relationships[*idx];
            for next in summary_relationship_connected_ids(r) {
                if !next.is_empty() && !seen_ids.contains(&next) {
                    queue.push_back(next);
                }
            }
            out.insert(summary_relationship_instance_key(r), r.clone());
        }
    }
    out.into_values().collect()
}

fn summary_relationship_connected_ids(r: &SessionRelationshipRecord) -> Vec<String> {
    let mut ids = vec![r.session_id.clone()];
    if let Some(related) = &r.related_session_id {
        ids.push(related.clone());
    }
    if let Some(agent) = &r.agent_id {
        ids.push(agent.clone());
    }
    ids
}

fn load_summary_subagent_tree_turns(
    ledger: &crate::ledger::Ledger,
    session_id: &str,
    relationships: &[SessionRelationshipRecord],
    q: &Query,
) -> Result<Vec<EnrichedTurn>> {
    let mut session_ids = HashSet::from([session_id.to_string()]);
    for r in relationships {
        session_ids.insert(r.session_id.clone());
    }

    let mut by_key: IndexMap<String, EnrichedTurn> = IndexMap::new();
    for id in session_ids {
        let turns = ledger.query_turns(&Query {
            session_id: Some(id),
            ..q.clone()
        })?;
        for t in turns {
            let key = format!(
                "{}|{}|{}",
                t.turn.source.wire_str(),
                t.turn.session_id,
                t.turn.message_id,
            );
            by_key.insert(key, t);
        }
    }
    Ok(by_key.into_values().collect())
}

fn find_summary_tree_node<'a>(
    trees: impl IntoIterator<Item = &'a SubagentTreeNode>,
    node_id: &str,
) -> Option<SubagentTreeNode> {
    for root in trees {
        if let Some(found) = find_summary_node(root, node_id) {
            return Some(found.clone());
        }
    }
    None
}

fn find_summary_node<'a>(
    node: &'a SubagentTreeNode,
    node_id: &str,
) -> Option<&'a SubagentTreeNode> {
    if node.node_id == node_id {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_summary_node(child, node_id) {
            return Some(found);
        }
    }
    None
}
