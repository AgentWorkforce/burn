//! JSON serialization for `burn summary` reports.

use relayburn_sdk::{
    summary_fidelity_summary_to_value, summary_replacement_savings_to_value, CostBreakdown,
    StopReasonCounts, SummaryGroupBy, SummaryGroupedReport,
};
use serde_json::{json, Map, Value};

use crate::cli::GlobalArgs;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd};
use crate::render::json::render_json;

use super::*;

pub(super) fn emit_json(
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) -> std::io::Result<()> {
    let value = grouped_json_value(report, ingest_report);
    render_json(&value)
}

/// Render a `--bucket` time-series. JSON emits `{ bucketSeconds, buckets: [...] }`
/// (the consumer); human output is one line per bucket.
pub(super) fn emit_summary_timeseries(
    globals: &GlobalArgs,
    series: &relayburn_sdk::SummaryTimeseries,
    ingest_report: &relayburn_sdk::IngestReport,
) -> anyhow::Result<i32> {
    if globals.json {
        render_json(series)?;
        return Ok(0);
    }
    emit_human_ingest_prelude(ingest_report);
    if series.buckets.is_empty() {
        println!("(no data in range)");
        return Ok(0);
    }
    for bucket in &series.buckets {
        println!(
            "{}  {:>5} turns  {:>14} tok  {}",
            bucket.start,
            bucket.turn_count,
            format_uint(bucket.total_tokens),
            format_usd(bucket.total_cost.total),
        );
    }
    Ok(0)
}

pub(super) fn grouped_json_value(
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) -> Value {
    let key = report.group_by.json_key();
    let label_key = report.group_by.wire_str();

    let group_rows: Vec<Value> = report
        .rows
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let mut row = if report.group_by == SummaryGroupBy::Tag {
                json!({
                    "tag": report.tag_key.as_deref().unwrap_or(""),
                    "value": report.tag_values.get(idx).cloned().flatten(),
                })
            } else {
                json!({
                    label_key: r.label,
                })
            };
            let obj = row.as_object_mut().unwrap();
            obj.insert("turns".into(), json!(r.turns));
            obj.insert(
                "usage".into(),
                json!({
                    "input": r.usage.input,
                    "output": r.usage.output,
                    "reasoning": r.usage.reasoning,
                    "cacheRead": r.usage.cache_read,
                    "cacheCreate5m": r.usage.cache_create_5m,
                    "cacheCreate1h": r.usage.cache_create_1h,
                }),
            );
            obj.insert("cost".into(), cost_breakdown_to_json(&r.cost));
            row
        })
        .collect();

    let mut payload = Map::new();
    payload.insert(
        "ingest".into(),
        json!({
            "ingestedSessions": ingest_report.ingested_sessions,
            "appendedTurns": ingest_report.appended_turns,
        }),
    );
    payload.insert("turns".into(), json!(report.turn_count));
    payload.insert(
        "totalCost".into(),
        cost_breakdown_to_json(&report.total_cost),
    );
    payload.insert(key.into(), Value::Array(group_rows));
    payload.insert(
        "fidelity".into(),
        json!({
            "summary": summary_fidelity_summary_to_value(&report.fidelity),
            "perCell": report.per_cell_fidelity.clone(),
        }),
    );
    if report.replacement_savings.calls > 0 {
        payload.insert(
            "replacementSavings".into(),
            summary_replacement_savings_to_value(&report.replacement_savings),
        );
    }
    payload.insert(
        "stopReasons".into(),
        stop_reasons_to_json(&report.stop_reasons),
    );
    if !report.subagents.is_empty() {
        // `subagents: {paired, orphan, total}` (issue #435). Skipped
        // when both buckets are zero so the JSON shape stays compact
        // for sessions that never spawned a subagent.
        payload.insert(
            "subagents".into(),
            json!({
                "paired": report.subagents.paired,
                "orphan": report.subagents.orphan,
                "total": report.subagents.total(),
            }),
        );
    }
    if let Some(quality) = report.quality.as_ref() {
        payload.insert("quality".into(), json!(quality));
    }

    let mut value = Value::Object(payload);
    coerce_whole_f64_to_int(&mut value);
    value
}

pub(super) fn cost_breakdown_to_json(c: &CostBreakdown) -> Value {
    json!({
        "model": c.model.as_ref(),
        "total": c.total,
        "input": c.input,
        "output": c.output,
        "reasoning": c.reasoning,
        "cacheRead": c.cache_read,
        "cacheCreate": c.cache_create,
    })
}

/// JSON shape for the outcome breakdown. Keys are camelCase to match the
/// rest of the summary surface; every bucket is emitted unconditionally so
/// downstream consumers can index keys without `?` plumbing.
pub(super) fn stop_reasons_to_json(s: &StopReasonCounts) -> Value {
    json!({
        "endTurn": s.end_turn,
        "maxTokens": s.max_tokens,
        "pauseTurn": s.pause_turn,
        "stopSequence": s.stop_sequence,
        "toolUse": s.tool_use,
        "refusal": s.refusal,
        "silent": s.silent,
        "none": s.none,
    })
}
