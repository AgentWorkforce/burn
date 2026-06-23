//! JSON serialization for `burn hotspots` results.

use relayburn_sdk::{
    AttributionMethod, BashAggregation, BashVerbAggregation, FileAggregation,
    HotspotsAttributionResult, HotspotsResult, HotspotsSessionTotal, McpServerAggregation,
    SubagentAggregation,
};
use serde_json::{json, Map, Value};

use crate::render::format::coerce_whole_f64_to_int;
use crate::render::json::render_json;

pub(super) fn emit_json(result: &HotspotsResult) -> std::io::Result<()> {
    let mut value = hotspots_result_to_json(result);
    coerce_whole_f64_to_int(&mut value);
    render_json(&value)
}

pub(super) fn hotspots_result_to_json(result: &HotspotsResult) -> Value {
    match result {
        HotspotsResult::Attribution(a) => attribution_to_json(a),
        HotspotsResult::Bash {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(bash_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::BashVerb {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(bash_verb_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::File {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(file_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::Subagent {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(subagent_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::Findings { findings, summary } => json!({
            "findings": findings,
            "summary": summary,
        }),
    }
}

pub(super) fn attribution_to_json(a: &HotspotsAttributionResult) -> Value {
    let mut out = Map::new();
    out.insert("turnsAnalyzed".into(), json!(a.turns_analyzed));
    out.insert("grandTotal".into(), json!(a.grand_total));
    out.insert("attributedTotal".into(), json!(a.attributed_total));
    out.insert("unattributedTotal".into(), json!(a.unattributed_total));
    out.insert("attributionDegraded".into(), json!(a.attribution_degraded));
    out.insert(
        "sessions".into(),
        Value::Array(a.sessions.iter().map(session_total_to_json).collect()),
    );
    out.insert(
        "files".into(),
        Value::Array(a.files.iter().map(file_to_json).collect()),
    );
    out.insert(
        "bashVerbs".into(),
        Value::Array(a.bash_verbs.iter().map(bash_verb_to_json).collect()),
    );
    out.insert(
        "bash".into(),
        Value::Array(a.bash.iter().map(bash_to_json).collect()),
    );
    out.insert(
        "subagents".into(),
        Value::Array(a.subagents.iter().map(subagent_to_json).collect()),
    );
    out.insert(
        "mcpServers".into(),
        Value::Array(a.mcp_servers.iter().map(mcp_server_to_json).collect()),
    );
    out.insert(
        "fidelity".into(),
        json!({
            "analyzed": a.fidelity.analyzed,
            "excluded": a.fidelity.excluded,
            "summary": reorder_fidelity_summary(&a.fidelity.summary),
            "refused": a.fidelity.refused,
        }),
    );
    if let Some(refused) = a.refused {
        out.insert("refused".into(), json!(refused));
    }
    if let Some(reason) = a.refusal_reason.as_ref() {
        out.insert("refusalReason".into(), json!(reason));
    }
    Value::Object(out)
}

pub(super) fn session_total_to_json(s: &HotspotsSessionTotal) -> Value {
    json!({
        "sessionId": s.session_id,
        "grandCost": s.grand_cost,
        "attributedCost": s.attributed_cost,
        "unattributedCost": s.unattributed_cost,
        "attributionMethod": attribution_method_key(s.attribution_method),
    })
}

/// Re-order the SDK-emitted fidelity summary so the JSON keys match the
/// TS-CLI snapshot ordering. The SDK builds `byClass` /
/// `byGranularity` / `missingCoverage` from `HashMap`s so iteration
/// order is non-deterministic; we reach into the `Value`, pull out the
/// numbers, and reassemble the object in the canonical order the TS
/// implementation uses (which is also the iteration order of the
/// upstream enum).
pub(super) fn reorder_fidelity_summary(summary: &Value) -> Value {
    use serde_json::Map;
    let Some(obj) = summary.as_object() else {
        return summary.clone();
    };
    let mut out = Map::new();
    out.insert(
        "total".into(),
        obj.get("total").cloned().unwrap_or(json!(0)),
    );

    let mut by_class = Map::new();
    let class_block = obj.get("byClass").and_then(|v| v.as_object());
    for key in [
        "full",
        "usage-only",
        "aggregate-only",
        "cost-only",
        "partial",
    ] {
        let v = class_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        by_class.insert(key.to_string(), v);
    }
    out.insert("byClass".into(), Value::Object(by_class));

    let mut by_granularity = Map::new();
    let gran_block = obj.get("byGranularity").and_then(|v| v.as_object());
    for key in [
        "per-turn",
        "per-message",
        "per-session-aggregate",
        "cost-only",
    ] {
        let v = gran_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        by_granularity.insert(key.to_string(), v);
    }
    out.insert("byGranularity".into(), Value::Object(by_granularity));

    let mut missing = Map::new();
    let missing_block = obj.get("missingCoverage").and_then(|v| v.as_object());
    for key in [
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
        let v = missing_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        missing.insert(key.to_string(), v);
    }
    out.insert("missingCoverage".into(), Value::Object(missing));
    out.insert(
        "unknown".into(),
        obj.get("unknown").cloned().unwrap_or(json!(0)),
    );
    Value::Object(out)
}

pub(super) fn attribution_method_key(m: AttributionMethod) -> &'static str {
    match m {
        AttributionMethod::Sized => "sized",
        AttributionMethod::EvenSplit => "even-split",
    }
}

pub(super) fn file_to_json(f: &FileAggregation) -> Value {
    json!({
        "path": f.path,
        "toolCallCount": f.tool_call_count,
        "initialTokens": f.initial_tokens,
        "persistenceTokens": f.persistence_tokens,
        "ridingTurns": f.riding_turns,
        "totalCost": f.total_cost,
        "firstEmitTs": f.first_emit_ts,
        "firstEmitTurnIndex": f.first_emit_turn_index,
        "totalOutputBytes": f.total_output_bytes,
        "maxOutputBytes": f.max_output_bytes,
        "truncatedCount": f.truncated_count,
    })
}

pub(super) fn bash_to_json(b: &BashAggregation) -> Value {
    let mut out = Map::new();
    out.insert("argsHash".into(), json!(b.args_hash));
    if let Some(c) = &b.command {
        out.insert("command".into(), json!(c));
    }
    out.insert("callCount".into(), json!(b.call_count));
    out.insert("totalCost".into(), json!(b.total_cost));
    out.insert("initialTokens".into(), json!(b.initial_tokens));
    out.insert("persistenceTokens".into(), json!(b.persistence_tokens));
    out.insert("totalOutputBytes".into(), json!(b.total_output_bytes));
    out.insert("maxOutputBytes".into(), json!(b.max_output_bytes));
    out.insert("truncatedCount".into(), json!(b.truncated_count));
    Value::Object(out)
}

pub(super) fn bash_verb_to_json(b: &BashVerbAggregation) -> Value {
    json!({
        "verb": b.verb,
        "callCount": b.call_count,
        "distinctCommands": b.distinct_commands,
        "totalCost": b.total_cost,
        "initialTokens": b.initial_tokens,
        "persistenceTokens": b.persistence_tokens,
        "avgPersistenceTurns": b.avg_persistence_turns,
        "topExamples": b.top_examples,
        "totalOutputBytes": b.total_output_bytes,
        "maxOutputBytes": b.max_output_bytes,
        "truncatedCount": b.truncated_count,
    })
}

pub(super) fn subagent_to_json(s: &SubagentAggregation) -> Value {
    json!({
        "subagentType": s.subagent_type,
        "callCount": s.call_count,
        "totalCost": s.total_cost,
        "initialTokens": s.initial_tokens,
        "persistenceTokens": s.persistence_tokens,
        "totalOutputBytes": s.total_output_bytes,
        "maxOutputBytes": s.max_output_bytes,
        "truncatedCount": s.truncated_count,
    })
}

pub(super) fn mcp_server_to_json(m: &McpServerAggregation) -> Value {
    json!({
        "server": m.server,
        "callCount": m.call_count,
        "initialTokens": m.initial_tokens,
        "persistenceTokens": m.persistence_tokens,
        "ridingTurns": m.riding_turns,
        "totalCost": m.total_cost,
        "topTools": m.top_tools,
    })
}
