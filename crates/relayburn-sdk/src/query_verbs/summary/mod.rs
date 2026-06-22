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

pub(crate) fn validate_tags(tags: &Enrichment) -> Result<()> {
    for key in tags.keys() {
        validate_tag_key(key, "tag")?;
    }
    Ok(())
}

pub(crate) fn validate_tag_key(key: &str, label: &str) -> Result<()> {
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

        let Some((buckets, per_bucket)) =
            super::partition_into_buckets(turns, q.since.as_deref(), bucket_secs, |t| &t.ts)?
        else {
            return Ok(SummaryTimeseries {
                bucket_secs,
                buckets: Vec::new(),
            });
        };

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

mod compute;
pub(crate) use compute::*;
