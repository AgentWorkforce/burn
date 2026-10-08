use super::*;

mod compute;
pub(crate) use compute::{
    detect_hotspots, hotspots_attribution, HotspotDetections, HotspotEnvironment,
    HotspotSideRecords, SessionDetail,
};

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
    "unpriced-usage",
];

pub(crate) fn default_hotspots_finding_kinds() -> Vec<String> {
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
        let pricing = load_pricing_for_ledger(self);
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
        let user_turns = self.inner.query_user_turns(&side_q)?;
        let tool_result_events = self.inner.query_tool_result_events(&side_q)?;
        let side = HotspotSideRecords {
            user_turns: &user_turns,
            tool_result_events: &tool_result_events,
            session_detail: None,
        };

        let wanted = match (opts.group_by, opts.patterns) {
            (Some(HotspotsGroupBy::Findings), Some(p)) if !p.is_empty() => p,
            (Some(HotspotsGroupBy::Findings), _) => default_hotspots_finding_kinds(),
            (_, Some(p)) if !p.is_empty() => p,
            _ => return Ok(hotspots_attribution(&turns, &side, &pricing, opts.group_by)),
        };
        let wanted: HashSet<String> = wanted.into_iter().collect();
        let detections = detect_hotspots(
            &turns,
            &side,
            &pricing,
            &HotspotEnvironment {
                settings: &ledger_claude_settings(),
                ghost_surface: wanted
                    .contains("ghost-surface")
                    .then(|| build_ghost_surface_inputs(&turns, &pricing, None)),
                wanted: &wanted,
            },
        );
        Ok(HotspotsResult::Findings {
            findings: detections.waste_findings(&turns, &pricing, &wanted),
            summary: fidelity_summary_to_value(&summarize_fidelity(&turns)),
        })
    }
}

pub fn hotspots(opts: HotspotsOptions) -> Result<HotspotsResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.hotspots(HotspotsOptions {
        ledger_home: None,
        ..opts
    })
}

/// The user-level and working-directory Claude settings the cross-session
/// tool-output-bloat check reads.
fn ledger_claude_settings() -> Vec<LoadedClaudeSettings> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    [
        load_claude_settings(user_claude_settings_path()),
        load_claude_settings(project_claude_settings_path(&cwd)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Folds the coverage gap on `t` into the per-source breakdown. Mirrors
/// the CLI-side `describeExcluded` from `packages/cli/src/commands/hotspots.ts`
/// so callers can render the inline source clause without a second ledger
/// walk. Turns without `fidelity` are treated as best-effort full upstream
/// (`turn_passes_hotspots_coverage`) and never reach this function.
pub(crate) fn record_excluded_source(out: &mut HotspotsExcludedBreakdown, t: &TurnRecord) {
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

pub(crate) fn refused_for_group(
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

pub(crate) fn parse_bash_verb(command: &str) -> Option<BashParse> {
    parse_bash_command(command)
}

pub(crate) fn fidelity_summary_to_value(s: &FidelitySummary) -> serde_json::Value {
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
