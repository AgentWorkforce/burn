//! Private compute engine for the `summary` / `summary_report` verbs.
//!
//! This module holds the query-building and aggregation helpers that the
//! `LedgerHandle::summary_report` / `summary_timeseries` dispatchers in
//! `super` drive. The public option/report types and the dispatchers
//! themselves live in `super` (`query_verbs/summary/mod.rs`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use anyhow::Result;
use indexmap::IndexMap;

use crate::analyze::{
    compute_quality, cost_for_turn, provider_for, summarize_fidelity,
    summarize_replacement_savings, ComputeQualityOptions, CostBreakdown, CoverageField,
    FieldCoverage, PricingTable, ProviderAggregateRow, ProviderFilter, QualityResult, RowCoverage,
    SubagentTreeNode, UsageCostAggregateRow,
};
use crate::ledger::{EnrichedTurn, Query};
use crate::reader::{
    ContentRecord, Coverage, RelationshipType, SessionRelationshipRecord, TurnRecord, Usage,
    UserTurnBlockKind, UserTurnRecord,
};

use super::super::build_query;
use super::*;

pub(crate) fn build_summary_report_query(opts: &SummaryReportOptions) -> Result<Query> {
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

pub(crate) fn normalize_summary_provider_filter(
    providers: Option<&[String]>,
) -> Option<ProviderFilter> {
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

pub(crate) fn filter_summary_enriched_turns(
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

pub(crate) fn summary_agent_passes(
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

pub(crate) fn summary_provider_passes(
    t: &TurnRecord,
    provider_filter: Option<&ProviderFilter>,
) -> bool {
    let Some(filter) = provider_filter else {
        return true;
    };
    let provider = provider_for(t).provider.to_ascii_lowercase();
    filter.contains(&provider)
}

pub(crate) fn summary_turns_from_enriched(enriched: &[EnrichedTurn]) -> Vec<TurnRecord> {
    enriched.iter().map(|e| e.turn.clone()).collect()
}

pub(crate) fn load_summary_by_tool_attribution_turns(
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

pub(crate) fn resolve_summary_agent_session_tree(
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
pub(crate) fn compute_summary_subagent_counts(
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

pub(crate) fn compute_summary_quality_for_turns(
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

pub(crate) fn load_summary_content_for_quality(
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

pub(crate) fn summary_aggregate_by_tag(
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

pub(crate) fn summary_aggregate_by_model(
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

pub(crate) fn summary_provider_to_aggregate_row(p: ProviderAggregateRow) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: p.label,
        turns: p.turns,
        usage: p.usage,
        cost: p.cost,
        coverage: p.coverage,
    }
}

pub(crate) fn summary_empty_row(label: &str) -> UsageCostAggregateRow {
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

pub(crate) fn summary_accumulate_coverage(target: &mut RowCoverage, coverage: Option<&Coverage>) {
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

pub(crate) fn summary_cell_is_partial(c: &FieldCoverage) -> bool {
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
pub(crate) struct SummaryUserTurnSizeBucket {
    tool_bytes_by_id: HashMap<String, u64>,
    total_bytes: u64,
}

pub(crate) fn compute_summary_by_tool_report(
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

pub(crate) fn load_summary_user_turns_for_by_tool(
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

pub(crate) fn selected_summary_turn_keys(turns: &[TurnRecord]) -> HashSet<String> {
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

pub(crate) fn summary_turn_is_selected(
    turn: &TurnRecord,
    selected_turns: Option<&HashSet<String>>,
) -> bool {
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

pub(crate) fn index_summary_user_turn_block_sizes(
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

pub(crate) fn summary_bridge_key(preceding_message_id: &str, following_message_id: &str) -> String {
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

pub(crate) const SUMMARY_RELATIONSHIP_ORDER: [RelationshipType; 4] = [
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

pub(crate) struct SummaryRelationshipTurnIndex<'a> {
    all_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    main_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    sidechain_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    subagent_by_session_agent: HashMap<String, Vec<&'a TurnRecord>>,
}

pub(crate) fn summary_relationship_query_for_turn_slice(q: &Query) -> Query {
    Query {
        session_id: q.session_id.clone(),
        source: q.source,
        ..Default::default()
    }
}

pub(crate) fn match_summary_relationships_to_turns(
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

pub(crate) fn build_summary_relationship_turn_index(
    turns: &[TurnRecord],
) -> SummaryRelationshipTurnIndex<'_> {
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

pub(crate) fn summary_turns_for_relationship<'a>(
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

pub(crate) fn aggregate_summary_relationship_subagent_stats(
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

pub(crate) fn summary_relationship_subagent_type(
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

pub(crate) fn summary_relationship_instance_key(r: &SessionRelationshipRecord) -> String {
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

pub(crate) fn summary_session_agent_key(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}\0{agent_id}")
}

pub(crate) fn summary_is_main_thread_turn(turn: &TurnRecord) -> bool {
    match &turn.subagent {
        None => true,
        Some(sub) => !sub.is_sidechain || sub.agent_id.as_deref() == Some(&turn.session_id),
    }
}

pub(crate) fn summary_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank =
        ((p * sorted.len() as f64).ceil() as i64 - 1).clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[rank]
}

pub(crate) fn collect_summary_subagent_tree_relationships(
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

pub(crate) fn summary_relationship_connected_ids(r: &SessionRelationshipRecord) -> Vec<String> {
    let mut ids = vec![r.session_id.clone()];
    if let Some(related) = &r.related_session_id {
        ids.push(related.clone());
    }
    if let Some(agent) = &r.agent_id {
        ids.push(agent.clone());
    }
    ids
}

pub(crate) fn load_summary_subagent_tree_turns(
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

pub(crate) fn find_summary_tree_node<'a>(
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

pub(crate) fn find_summary_node<'a>(
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
