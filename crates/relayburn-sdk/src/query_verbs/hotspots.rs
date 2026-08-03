use super::*;

// ---------------------------------------------------------------------------
// hotspots — discriminated union
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotspotsGroupBy {
    Attribution,
    Bash,
    BashVerb,
    File,
    Subagent,
    Findings,
}

const DEFAULT_HOTSPOTS_FINDING_KINDS: &[&str] = &[
    "retry-loop",
    "failure-run",
    "cancellation-run",
    "compaction-loss",
    "edit-revert",
    "edit-heavy",
    "skill-recall-dup",
    "skill-pruning-protection",
    "system-prompt-tax",
    "ghost-surface",
    "tool-output-bloat",
    "tool-call-pattern",
];

fn default_hotspots_finding_kinds() -> Vec<String> {
    DEFAULT_HOTSPOTS_FINDING_KINDS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub group_by: Option<HotspotsGroupBy>,
    pub patterns: Option<Vec<String>>,
    /// Restrict to turns whose `enrichment.workflowId` matches.
    pub workflow: Option<String>,
    /// Restrict to turns whose derived provider is in the given set
    /// (case-insensitive). `None` / empty = no provider filter.
    pub provider: Option<Vec<String>>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsSessionTotal {
    pub session_id: String,
    pub grand_cost: f64,
    pub attributed_cost: f64,
    pub unattributed_cost: f64,
    pub attribution_method: AttributionMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsFidelityBlock {
    pub analyzed: u64,
    pub excluded: u64,
    /// Aggregate fidelity summary for the matched-window turns. Stored as a
    /// `serde_json::Value` because older hotspot result shapes already exposed
    /// this JSON block directly.
    pub summary: serde_json::Value,
    pub refused: bool,
    /// Per-source coverage-gap breakdown. Computed in the same pass as the
    /// eligible/excluded split so CLI/MCP renderers don't need to re-walk the
    /// ledger to recover *which* sources contributed excluded turns. Not
    /// serialized — the JSON contract owns the aggregate counts above; this
    /// is an in-process renderer aid.
    #[serde(skip)]
    pub excluded_by_source: HotspotsExcludedBreakdown,
}

/// Per-source breakdown of turns that failed the hotspots coverage gate.
/// Sources are keyed by their wire string (e.g. `claude`, `codex`,
/// `opencode`) so the renderer can produce stable ordering without a second
/// ledger walk. See `HotspotsFidelityBlock::excluded_by_source`.
#[derive(Debug, Clone, Default)]
pub struct HotspotsExcludedBreakdown {
    pub sources: BTreeMap<String, HotspotsExcludedSourceRow>,
}

#[derive(Debug, Clone, Default)]
pub struct HotspotsExcludedSourceRow {
    pub count: u64,
    /// Distinct missing-coverage labels (e.g. `tool-call records`,
    /// `tool-result events`).
    pub missing: BTreeSet<String>,
    /// Distinct granularity buckets observed on excluded turns from this
    /// source.
    pub granularities: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum HotspotsResult {
    #[serde(rename = "attribution")]
    Attribution(Box<HotspotsAttributionResult>),
    #[serde(rename = "bash")]
    Bash {
        rows: Vec<BashAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "bash-verb")]
    BashVerb {
        rows: Vec<BashVerbAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "file")]
    File {
        rows: Vec<FileAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "subagent")]
    Subagent {
        rows: Vec<SubagentAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "findings")]
    Findings {
        findings: Vec<WasteFinding>,
        summary: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsAttributionResult {
    pub turns_analyzed: u64,
    pub grand_total: f64,
    pub attributed_total: f64,
    pub unattributed_total: f64,
    pub attribution_degraded: bool,
    pub sessions: Vec<HotspotsSessionTotal>,
    pub files: Vec<FileAggregation>,
    pub bash_verbs: Vec<BashVerbAggregation>,
    pub bash: Vec<BashAggregation>,
    pub subagents: Vec<SubagentAggregation>,
    pub mcp_servers: Vec<McpServerAggregation>,
    pub fidelity: HotspotsFidelityBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,
}

impl LedgerHandle {
    pub fn hotspots(&self, opts: HotspotsOptions) -> Result<HotspotsResult> {
        let using_patterns = opts
            .patterns
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        if let Some(workflow) = opts.workflow.as_ref() {
            let mut enrichment = q.enrichment.unwrap_or_default();
            enrichment.insert("workflowId".to_string(), workflow.clone());
            q.enrichment = Some(enrichment);
        }
        let mut turns = collect_turns(self, &q)?;
        if let Some(filter) = normalize_provider_filter(opts.provider.clone()) {
            turns.retain(|t| {
                let provider = crate::analyze::provider_for(t).provider;
                filter.contains(&provider.to_ascii_lowercase())
            });
        }
        let pricing = load_pricing(None);

        if matches!(opts.group_by, Some(HotspotsGroupBy::Findings)) {
            let patterns = match opts.patterns {
                Some(patterns) if !patterns.is_empty() => patterns,
                _ => default_hotspots_finding_kinds(),
            };
            return run_hotspots_findings(self, &turns, &pricing, patterns, &q);
        }
        if using_patterns {
            return run_hotspots_findings(
                self,
                &turns,
                &pricing,
                opts.patterns.unwrap_or_default(),
                &q,
            );
        }
        run_hotspots_attribution(self, &turns, &pricing, opts.group_by, &q)
    }
}

pub fn hotspots(opts: HotspotsOptions) -> Result<HotspotsResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.hotspots(HotspotsOptions {
        ledger_home: None,
        ..opts
    })
}

fn run_hotspots_attribution(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
    group_by: Option<HotspotsGroupBy>,
    q: &Query,
) -> Result<HotspotsResult> {
    let mut eligible: Vec<TurnRecord> = Vec::new();
    let mut excluded: Vec<TurnRecord> = Vec::new();
    let mut excluded_by_source = HotspotsExcludedBreakdown::default();
    for t in turns {
        if turn_passes_hotspots_coverage(t) {
            eligible.push(t.clone());
        } else {
            record_excluded_source(&mut excluded_by_source, t);
            excluded.push(t.clone());
        }
    }
    let fidelity_summary = summarize_fidelity(turns);
    let summary_value = fidelity_summary_to_value(&fidelity_summary);

    if !turns.is_empty() && eligible.is_empty() {
        let refusal = format!(
            "{}/{} turns lack tool-call/tool-result coverage required for hotspots attribution",
            turns.len(),
            turns.len()
        );
        let group = group_by.unwrap_or(HotspotsGroupBy::Attribution);
        return Ok(refused_for_group(
            group,
            refusal,
            turns.len() as u64,
            summary_value,
            excluded_by_source,
        ));
    }

    let session_ids: HashSet<String> = eligible.iter().map(|t| t.session_id.clone()).collect();
    // Propagate `enrichment` (e.g. workflowId folds) into side queries so a
    // partial-session workflow stamp doesn't pull unrelated user-turns /
    // tool-result events into the per-session buckets and skew attribution
    // outside the requested slice.
    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
        enrichment: q.enrichment.clone(),
        ..Default::default()
    };
    let user_turns_by_session = bucket_user_turns_by_session(handle, &side_q, Some(&session_ids))?;
    // Bytes plumbing (#436): hand attribute_hotspots a per-session lookup
    // so it can stamp `output_bytes` / `output_truncated` onto each
    // attribution row from the matching `ToolResultEventRecord`.
    let tool_result_events_by_session =
        bucket_tool_result_events_by_session(handle, &side_q, Some(&session_ids))?;

    let result = attribute_hotspots(
        &eligible,
        &AnalyzeHotspotsOptions {
            pricing,
            content_by_session: None,
            user_turns_by_session: Some(&user_turns_by_session),
            tool_result_events_by_session: Some(&tool_result_events_by_session),
        },
    );

    let group = group_by.unwrap_or(HotspotsGroupBy::Attribution);
    match group {
        HotspotsGroupBy::Bash => {
            return Ok(HotspotsResult::Bash {
                rows: aggregate_by_bash(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::BashVerb => {
            return Ok(HotspotsResult::BashVerb {
                rows: aggregate_by_bash_verb(&result.attributions, parse_bash_verb),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::File => {
            return Ok(HotspotsResult::File {
                rows: aggregate_by_file(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::Subagent => {
            return Ok(HotspotsResult::Subagent {
                rows: aggregate_by_subagent(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::Findings => unreachable!("findings is handled before attribution"),
        HotspotsGroupBy::Attribution => {}
    }

    let files = aggregate_by_file(&result.attributions);
    let bash_verbs = aggregate_by_bash_verb(&result.attributions, parse_bash_verb);
    let bash = aggregate_by_bash(&result.attributions);
    let subagents = aggregate_by_subagent(&result.attributions);
    let mcp_servers = aggregate_by_mcp_server(&result.attributions);
    let even_split: usize = result
        .session_totals
        .iter()
        .filter(|s| matches!(s.attribution_method, AttributionMethod::EvenSplit))
        .count();
    let degraded = !result.session_totals.is_empty()
        && (even_split as f64 / result.session_totals.len() as f64) >= 0.5;

    let sessions = result
        .session_totals
        .into_iter()
        .map(|s| HotspotsSessionTotal {
            session_id: s.session_id,
            grand_cost: s.grand_cost,
            attributed_cost: s.attributed_cost,
            unattributed_cost: s.unattributed_cost,
            attribution_method: s.attribution_method,
        })
        .collect();

    Ok(HotspotsResult::Attribution(Box::new(
        HotspotsAttributionResult {
            turns_analyzed: eligible.len() as u64,
            grand_total: result.grand_total,
            attributed_total: result.attributed_total,
            unattributed_total: result.unattributed_total,
            attribution_degraded: degraded,
            sessions,
            files,
            bash_verbs,
            bash,
            subagents,
            mcp_servers,
            fidelity: HotspotsFidelityBlock {
                analyzed: eligible.len() as u64,
                excluded: excluded.len() as u64,
                summary: summary_value,
                refused: false,
                excluded_by_source,
            },
            refused: None,
            refusal_reason: None,
        },
    )))
}

/// Folds the coverage gap on `t` into the per-source breakdown. Mirrors
/// the CLI-side `describeExcluded` from `packages/cli/src/commands/hotspots.ts`
/// so callers can render the inline source clause without a second ledger
/// walk. Turns without `fidelity` are treated as best-effort full upstream
/// (`turn_passes_hotspots_coverage`) and never reach this function.
fn record_excluded_source(out: &mut HotspotsExcludedBreakdown, t: &TurnRecord) {
    let entry = out
        .sources
        .entry(t.source.wire_str().to_string())
        .or_default();
    entry.count += 1;
    if let Some(f) = t.fidelity.as_ref() {
        if !f.coverage.has_tool_calls {
            entry.missing.insert("tool-call records".to_string());
        }
        if !f.coverage.has_tool_result_events {
            entry.missing.insert("tool-result events".to_string());
        }
        entry
            .granularities
            .insert(f.granularity.wire_str().to_string());
    }
}

fn refused_for_group(
    group: HotspotsGroupBy,
    refusal: String,
    excluded_total: u64,
    summary_value: serde_json::Value,
    excluded_by_source: HotspotsExcludedBreakdown,
) -> HotspotsResult {
    match group {
        HotspotsGroupBy::Bash => HotspotsResult::Bash {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::BashVerb => HotspotsResult::BashVerb {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::File => HotspotsResult::File {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::Subagent => HotspotsResult::Subagent {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::Findings => HotspotsResult::Findings {
            findings: Vec::new(),
            summary: summary_value,
        },
        HotspotsGroupBy::Attribution => {
            HotspotsResult::Attribution(Box::new(HotspotsAttributionResult {
                turns_analyzed: 0,
                grand_total: 0.0,
                attributed_total: 0.0,
                unattributed_total: 0.0,
                attribution_degraded: false,
                sessions: Vec::new(),
                files: Vec::new(),
                bash_verbs: Vec::new(),
                bash: Vec::new(),
                subagents: Vec::new(),
                mcp_servers: Vec::new(),
                fidelity: HotspotsFidelityBlock {
                    analyzed: 0,
                    excluded: excluded_total,
                    summary: summary_value,
                    refused: true,
                    excluded_by_source,
                },
                refused: Some(true),
                refusal_reason: Some(refusal),
            }))
        }
    }
}

fn parse_bash_verb(command: &str) -> Option<BashParse> {
    parse_bash_command(command)
}

fn run_hotspots_findings(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
    wanted: Vec<String>,
    q: &Query,
) -> Result<HotspotsResult> {
    let wanted_set: HashSet<String> = wanted.into_iter().collect();
    let mut findings: Vec<WasteFinding> = Vec::new();

    // Propagate `enrichment` (e.g. workflowId folds) into side queries so a
    // partial-session workflow stamp doesn't pull unrelated user-turns /
    // tool-result events into the per-session buckets and skew attribution
    // outside the requested slice.
    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
        enrichment: q.enrichment.clone(),
        ..Default::default()
    };

    let user_turns_all: Vec<UserTurnRecord> = handle.inner.query_user_turns(&side_q)?;
    let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
    for ut in &user_turns_all {
        user_turns_by_session
            .entry(ut.session_id.clone())
            .or_default()
            .push(ut.clone());
    }

    let detected = detect_patterns(
        turns,
        &DetectPatternsOptions {
            pricing,
            compactions: None,
            user_turns_by_session: Some(&user_turns_by_session),
            content_by_session: None,
            tool_result_events: None,
        },
    );
    for f in findings_from_patterns(&detected) {
        if wanted_set.contains(&f.kind) {
            findings.push(f);
        }
    }

    if wanted_set.contains("tool-output-bloat") {
        let mut settings: Vec<LoadedClaudeSettings> = Vec::new();
        if let Some(s) = load_claude_settings(user_claude_settings_path()) {
            settings.push(s);
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        if let Some(s) = load_claude_settings(project_claude_settings_path(&cwd)) {
            settings.push(s);
        }
        let tool_result_events = handle.inner.query_tool_result_events(&side_q)?;
        let bloats = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &tool_result_events,
            user_turns: &user_turns_all,
            turns,
            pricing,
            threshold: None,
            min_occurrences: None,
        });
        for b in bloats {
            findings.push(tool_output_bloat_to_finding(&b));
        }
    }

    if wanted_set.contains("ghost-surface") {
        let inputs = build_ghost_surface_inputs(turns, pricing, None);
        let ghosts = detect_ghost_surface(&inputs);
        let options = GhostSurfaceFindingOptions::default();
        for g in ghosts {
            findings.push(ghost_surface_to_finding(&g, &options));
        }
    }

    if wanted_set.contains("tool-call-pattern") {
        let patterns = detect_tool_call_patterns(turns, &DetectToolCallPatternsOptions { pricing });
        for p in patterns {
            findings.push(tool_call_pattern_to_finding(&p));
        }
    }

    // `findings_from_patterns` already sorts the slice it returns, but the
    // tool-output-bloat / ghost-surface / tool-call-pattern batches above
    // are appended afterwards. Re-sort once so the global slice is
    // severity-descending → usdPerSession-descending end-to-end (TS parity).
    sort_findings(&mut findings);

    Ok(HotspotsResult::Findings {
        findings,
        summary: fidelity_summary_to_value(&summarize_fidelity(turns)),
    })
}

fn fidelity_summary_to_value(s: &FidelitySummary) -> serde_json::Value {
    // Mirror the TS shape: { total, byClass, byGranularity, missingCoverage,
    // unknown }. The analyze type doesn't derive Serialize so build it here.
    let by_class: serde_json::Map<String, serde_json::Value> = s
        .by_class
        .iter()
        .map(|(k, v)| {
            let key = serde_json::to_value(k)
                .ok()
                .and_then(|x| x.as_str().map(str::to_string))
                .unwrap_or_default();
            (key, serde_json::Value::from(*v))
        })
        .collect();
    let by_granularity: serde_json::Map<String, serde_json::Value> = s
        .by_granularity
        .iter()
        .map(|(k, v)| {
            let key = serde_json::to_value(k)
                .ok()
                .and_then(|x| x.as_str().map(str::to_string))
                .unwrap_or_default();
            (key, serde_json::Value::from(*v))
        })
        .collect();
    let missing: serde_json::Map<String, serde_json::Value> = s
        .missing_coverage
        .iter()
        .map(|(k, v)| ((*k).to_string(), serde_json::Value::from(*v)))
        .collect();
    serde_json::json!({
        "total": s.total,
        "byClass": serde_json::Value::Object(by_class),
        "byGranularity": serde_json::Value::Object(by_granularity),
        "missingCoverage": serde_json::Value::Object(missing),
        "unknown": s.unknown,
    })
}
