//! GitHub Copilot CLI OTEL span parser (AgentWorkforce/burn#14).
//!
//! Copilot CLI does not write a session log; it emits usage through its
//! OpenTelemetry file exporter when the user sets
//! `COPILOT_OTEL_FILE_EXPORTER_PATH` (auto-enables
//! `COPILOT_OTEL_EXPORTER_TYPE=file`). The exporter appends one JSON record
//! per line — a `chat` span per API call, an `invoke_agent` summary span per
//! turn, plus metrics and log records we ignore. Burn additionally globs
//! `$COPILOT_HOME/otel/*.jsonl` (default `~/.copilot/otel`) so a directory
//! pointed at by the exporter is covered too. Both are scanned only while
//! `COPILOT_OTEL_FILE_EXPORTER_PATH` is set — without it ingest is a
//! silent no-op (test roots can still inject files directly).
//!
//! Format reference: tokscale's `sessions/copilot.rs` and the GitHub Docs
//! Copilot CLI command reference. Notable shapes handled here:
//!
//! - `{"type":"span","traceId","spanId","name":"chat <model>",
//!    "startTime":[secs,nanos],"endTime":[secs,nanos],"attributes":{…}}`
//! - usage attrs `gen_ai.usage.{input,output}_tokens` (input is *inclusive*
//!   of cache reads), cache buckets under both the dotted semconv spelling
//!   (`gen_ai.usage.cache_read.input_tokens`) and the underscored variant
//!   the CLI actually emits (`gen_ai.usage.cache_read_input_tokens`), and
//!   reasoning under `gen_ai.usage.reasoning{,.output}_tokens`.
//! - session identity by priority: `gen_ai.conversation.id`,
//!   `copilot_chat.session_id`, `copilot_chat.chat_session_id`,
//!   `session.id`, `github.copilot.interaction_id`, `gen_ai.response.id`,
//!   then the OTEL `traceId`, then `"unknown"`.
//!
//! An `invoke_agent` span reports *totals across all turns* of its trace, so
//! it is only used as a fallback when no `chat` span was seen for the same
//! `traceId` (tracked across incremental passes in
//! [`CopilotResumeState::chat_trace_ids`]). Spans with zero total tokens are
//! dropped.
//!
//! Tool calls are not available: OTEL spans carry structured metrics, not
//! content, so the emitted [`TurnRecord`]s are usage-only
//! ([`UsageGranularity::PerMessage`] — one record per API call).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Map, Value};

use crate::reader::types::{
    Coverage, Fidelity, SourceKind, StopReason, TurnRecord, Usage, UsageGranularity,
};
use crate::util::time::format_iso_ms;

/// Cap on `chat_trace_ids` carried in the cursor so a long-lived export file
/// can't grow ingest state without bound. Fallback double-count protection
/// only needs traces whose `invoke_agent` summary hasn't flushed yet, which
/// is a working set of in-flight turns — far below this cap.
const CHAT_TRACE_ID_CAP: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct ParseCopilotIncrementalOptions {
    pub session_path: Option<String>,
    pub start_offset: Option<u64>,
    pub resume: Option<CopilotResumeState>,
    /// Timestamp fallback (file mtime, ms) for spans that carry no usable
    /// `startTime`/`endTime`. tokscale uses the same fallback.
    pub fallback_ts_ms: Option<i64>,
}

/// State carried across incremental passes over the same export file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopilotResumeState {
    /// Next `turn_index` per session id.
    pub session_turn_counts: BTreeMap<String, u64>,
    /// Trace ids known to contain at least one `chat` span; `invoke_agent`
    /// summaries for these traces are suppressed to avoid double counting.
    pub chat_trace_ids: Vec<String>,
}

/// Turns parsed from one incremental pass over a Copilot OTEL export,
/// plus the cursor state (`end_offset` + `resume`) the next pass continues
/// from. `end_offset` stops at the last complete line so a partial tail
/// still being flushed is re-read next pass.
#[derive(Debug, Clone, Default)]
pub struct ParseCopilotIncrementalResult {
    pub turns: Vec<TurnRecord>,
    pub end_offset: u64,
    pub resume: CopilotResumeState,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ParseCopilotResult {
    pub turns: Vec<TurnRecord>,
}

#[cfg(test)]
pub fn parse_copilot_file(path: &Path) -> std::io::Result<ParseCopilotResult> {
    let parsed = parse_copilot_otel_incremental(
        path,
        &ParseCopilotIncrementalOptions {
            session_path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        },
    )?;
    Ok(ParseCopilotResult {
        turns: parsed.turns,
    })
}

/// Incrementally parse a Copilot OTEL JSONL export starting at
/// `start_offset`, emitting one [`TurnRecord`] per usage-bearing span.
///
/// Reading stops at the last complete line: a trailing partial line (a
/// span still being flushed by the exporter) is left for the next pass and
/// `end_offset` points at its first byte.
pub fn parse_copilot_otel_incremental(
    path: &Path,
    opts: &ParseCopilotIncrementalOptions,
) -> std::io::Result<ParseCopilotIncrementalResult> {
    let start_offset = opts.start_offset.unwrap_or(0);
    let mut resume = opts.resume.clone().unwrap_or_default();
    let session_path = opts.session_path.clone();

    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;

    // Only consume up to the final newline; bytes after it are a partial
    // line still being written.
    let consumed = match buf.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    let end_offset = start_offset + consumed as u64;

    // A usage span does not always carry its own model/session — those can
    // arrive on a sibling span sharing the trace id — so collect per-trace
    // context for this chunk first, then resolve candidates against it.
    // (tokscale reads the whole file for this; our incremental cursor makes
    // the chunk boundary the practical horizon, which matches how spans for
    // one trace land within milliseconds of each other.)
    let mut trace_contexts: BTreeMap<String, TraceContext> = BTreeMap::new();
    let mut candidates: Vec<PendingCandidate> = Vec::new();

    // `consumed` ends on a newline, so every piece (including blanks)
    // occupies `len + 1` bytes of the file; track the absolute offset so
    // the `message_id` fallback below stays stable across passes.
    let mut line_offset = start_offset;
    for line in buf[..consumed].split(|b| *b == b'\n') {
        let this_offset = line_offset;
        line_offset += line.len() as u64 + 1;
        let trimmed = trim_ascii(line);
        if trimmed.is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_slice(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // lossy per line: one bad record never truncates the file
        };
        accumulate_trace_context(&mut trace_contexts, &record);
        if let Some(candidate) = candidate_from_record(&record, this_offset, opts.fallback_ts_ms) {
            candidates.push(candidate);
        }
    }

    // Suppression must not depend on record order: an `invoke_agent`
    // summary can precede its trace's `chat` spans in the same chunk
    // (exporter flush order isn't contractual), so collect this chunk's
    // chat traces before resolving anything.
    let chunk_chat_traces: BTreeSet<String> = candidates
        .iter()
        .filter(|c| c.kind == SpanKind::Chat)
        .filter_map(|c| c.trace_id.clone())
        .collect();

    let mut turns = Vec::new();
    for candidate in candidates {
        let Some(turn) = candidate.resolve(
            &trace_contexts,
            &chunk_chat_traces,
            &mut resume,
            session_path.as_deref(),
        ) else {
            continue;
        };
        turns.push(turn);
    }

    Ok(ParseCopilotIncrementalResult {
        turns,
        end_offset,
        resume,
    })
}

// ---------------------------------------------------------------------------
// Record classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpanKind {
    /// `gen_ai.operation.name == "chat"` — one span per API call.
    Chat,
    /// `gen_ai.operation.name == "invoke_agent"` — per-turn aggregate;
    /// fallback only, suppressed when the trace has chat spans.
    AgentSummary,
}

fn is_span_record(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some(t) => t == "span",
        // VS Code Copilot Chat exports omit `type`; infer span-ness from a
        // top-level `name` plus span identity. Harmless for the CLI lane.
        None => value.get("name").and_then(Value::as_str).is_some() && span_id(value).is_some(),
    }
}

fn classify_span(value: &Value, attributes: &Map<String, Value>) -> Option<SpanKind> {
    if !is_span_record(value) {
        return None;
    }
    let op = attr_str(attributes, "gen_ai.operation.name");
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    if op == Some("chat") || name.starts_with("chat ") {
        return Some(SpanKind::Chat);
    }
    if op == Some("invoke_agent") || name.starts_with("invoke_agent ") || name == "invoke_agent" {
        return Some(SpanKind::AgentSummary);
    }
    None
}

// ---------------------------------------------------------------------------
// Attribute extraction
// ---------------------------------------------------------------------------

fn attr_str<'a>(attributes: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    attributes.get(key).and_then(Value::as_str)
}

fn first_non_empty_attr<'a>(attributes: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| attributes.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_f64().map(|v| v as i64))
        .or_else(|| value.as_str().and_then(|v| v.parse::<i64>().ok()))
}

fn attr_i64(attributes: &Map<String, Value>, key: &str) -> i64 {
    attributes
        .get(key)
        .and_then(value_as_i64)
        .unwrap_or(0)
        .max(0)
}

fn attr_i64_first(attributes: &Map<String, Value>, keys: &[&str]) -> i64 {
    keys.iter()
        .map(|key| attr_i64(attributes, key))
        .find(|value| *value > 0)
        .unwrap_or(0)
}

const MODEL_ATTRS: &[&str] = &["gen_ai.response.model", "gen_ai.request.model"];

/// Model suffix of a `chat <model>` span name. The exporter always names
/// chat spans this way, so a usage-bearing span without model attributes
/// still identifies its model instead of falling back to `"unknown"`.
fn model_from_span_name(name: &str) -> Option<&str> {
    name.strip_prefix("chat ")
        .map(str::trim)
        .filter(|model| !model.is_empty())
}

/// Session id attribute priority, mirroring tokscale's `SESSION_ATTRS`.
const SESSION_ATTRS: &[&str] = &[
    "gen_ai.conversation.id",
    "copilot_chat.session_id",
    "copilot_chat.chat_session_id",
    "session.id",
    "github.copilot.interaction_id",
    "gen_ai.response.id",
];

fn best_session_attr(attributes: &Map<String, Value>) -> Option<&str> {
    SESSION_ATTRS
        .iter()
        .find_map(|key| attr_str(attributes, key))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn trace_id(value: &Value) -> Option<&str> {
    non_empty_id(value.get("traceId").and_then(Value::as_str)).or_else(|| {
        non_empty_id(
            value
                .get("spanContext")
                .and_then(|sc| sc.get("traceId"))
                .and_then(Value::as_str),
        )
    })
}

fn span_id(value: &Value) -> Option<&str> {
    non_empty_id(value.get("spanId").and_then(Value::as_str)).or_else(|| {
        non_empty_id(
            value
                .get("spanContext")
                .and_then(|sc| sc.get("spanId"))
                .and_then(Value::as_str),
        )
    })
}

/// Treat empty and all-zero W3C sentinel ids as absent.
fn non_empty_id(id: Option<&str>) -> Option<&str> {
    id.map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().any(|c| c != '0' && c != '-'))
}

/// `[seconds, nanos]` OTEL time pair, or a scalar whose unit is inferred
/// from magnitude (ns / µs / ms / s), or an ISO string.
fn timestamp_ms_from_value(value: &Value) -> Option<i64> {
    match value {
        Value::Array(parts) if !parts.is_empty() => {
            let secs = value_as_i64(&parts[0])?;
            let nanos = parts.get(1).and_then(value_as_i64).unwrap_or(0);
            Some(secs.saturating_mul(1_000).saturating_add(nanos / 1_000_000))
        }
        Value::String(s) => crate::util::time::parse_iso_ms(s),
        _ => {
            let raw = value_as_i64(value)?;
            Some(match raw {
                // ~1e18 → nanoseconds, ~1e15 → microseconds, ~1e12 → ms.
                n if n >= 1_000_000_000_000_000_000 => n / 1_000_000,
                n if n >= 1_000_000_000_000_000 => n / 1_000,
                n if n >= 1_000_000_000_000 => n,
                n => n.saturating_mul(1_000),
            })
        }
    }
}

fn timestamp_ms_from_record(value: &Value) -> Option<i64> {
    value
        .get("startTime")
        .and_then(timestamp_ms_from_value)
        .or_else(|| value.get("hrTime").and_then(timestamp_ms_from_value))
        .or_else(|| value.get("_hrTime").and_then(timestamp_ms_from_value))
        .or_else(|| value.get("timeUnixNano").and_then(timestamp_ms_from_value))
        .or_else(|| {
            // End-only span: back-calculate the start from the duration.
            let end = value.get("endTime").and_then(timestamp_ms_from_value)?;
            let start = value.get("startTime").and_then(timestamp_ms_from_value);
            Some(start.unwrap_or(end))
        })
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

// ---------------------------------------------------------------------------
// Trace context + candidate resolution
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TraceContext {
    model: Option<String>,
    session_id: Option<String>,
}

fn accumulate_trace_context(contexts: &mut BTreeMap<String, TraceContext>, record: &Value) {
    let Some(trace) = trace_id(record).map(str::to_string) else {
        return;
    };
    let attributes = record
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let ctx = contexts.entry(trace).or_default();
    if ctx.model.is_none() {
        ctx.model = first_non_empty_attr(&attributes, MODEL_ATTRS)
            .map(str::to_string)
            .or_else(|| {
                record
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(model_from_span_name)
                    .map(str::to_string)
            });
    }
    if ctx.session_id.is_none() {
        ctx.session_id = best_session_attr(&attributes).map(str::to_string);
    }
}

struct PendingCandidate {
    kind: SpanKind,
    trace_id: Option<String>,
    span_id: Option<String>,
    response_id: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    stop_reason: Option<StopReason>,
    ts_ms: i64,
    usage: Usage,
    /// Absolute byte offset of this span's line in the export file;
    /// fallback identity for spans with no `spanId`. Chunk-local indexes
    /// would restart at zero on every incremental pass and collide in the
    /// ledger's `(source, session_id, message_id)` key; the absolute
    /// offset is stable for a given file generation, so a re-read span
    /// dedups instead of dropping a later, distinct turn.
    line_offset: u64,
}

fn candidate_from_record(
    record: &Value,
    line_offset: u64,
    fallback_ts_ms: Option<i64>,
) -> Option<PendingCandidate> {
    let attributes = record.get("attributes").and_then(Value::as_object)?;
    let kind = classify_span(record, attributes)?;
    let span_name = record.get("name").and_then(Value::as_str).unwrap_or("");

    let input = attr_i64(attributes, "gen_ai.usage.input_tokens");
    let output = attr_i64(attributes, "gen_ai.usage.output_tokens");
    let cache_read = attr_i64_first(
        attributes,
        &[
            "gen_ai.usage.cache_read.input_tokens",
            "gen_ai.usage.cache_read_input_tokens",
        ],
    );
    let cache_write = attr_i64_first(
        attributes,
        &[
            "gen_ai.usage.cache_write.input_tokens",
            "gen_ai.usage.cache_creation.input_tokens",
            "gen_ai.usage.cache_write_input_tokens",
            "gen_ai.usage.cache_creation_input_tokens",
        ],
    );
    let reasoning = attr_i64_first(
        attributes,
        &[
            "gen_ai.usage.reasoning.output_tokens",
            "gen_ai.usage.reasoning_tokens",
        ],
    );
    if input + output + cache_read + cache_write + reasoning == 0 {
        return None;
    }

    // OTEL reports input_tokens inclusive of cache reads; burn's
    // `Usage.input` is exclusive (same convention as the Claude/Codex
    // readers), so subtract the cached-read portion while keeping the
    // cache buckets intact for pricing.
    let cache_read_in_input = cache_read.min(input);
    let usage = Usage {
        input: (input - cache_read_in_input) as u64,
        output: output as u64,
        reasoning: reasoning as u64,
        cache_read: cache_read as u64,
        // Copilot doesn't split cache creation by TTL; fold into the 5m bucket.
        cache_create_5m: cache_write as u64,
        cache_create_1h: 0,
    };

    let ts_ms = timestamp_ms_from_record(record)
        .or(fallback_ts_ms)
        .unwrap_or(0);

    let stop_reason = record
        .get("attributes")
        .and_then(|a| a.get("gen_ai.response.finish_reasons"))
        .and_then(|v| match v {
            Value::Array(items) => items.first().and_then(Value::as_str),
            Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .and_then(StopReason::from_wire);

    Some(PendingCandidate {
        kind,
        trace_id: trace_id(record).map(str::to_string),
        span_id: span_id(record).map(str::to_string),
        response_id: attr_str(attributes, "gen_ai.response.id")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        model: first_non_empty_attr(attributes, MODEL_ATTRS)
            .map(str::to_string)
            .or_else(|| model_from_span_name(span_name).map(str::to_string)),
        session_id: best_session_attr(attributes).map(str::to_string),
        stop_reason,
        ts_ms,
        usage,
        line_offset,
    })
}

impl PendingCandidate {
    fn resolve(
        self,
        trace_contexts: &BTreeMap<String, TraceContext>,
        chunk_chat_traces: &BTreeSet<String>,
        resume: &mut CopilotResumeState,
        session_path: Option<&str>,
    ) -> Option<TurnRecord> {
        let ctx = self.trace_id.as_deref().and_then(|t| trace_contexts.get(t));

        // Aggregate `invoke_agent` spans double count when the same trace
        // already produced `chat` spans — whether in this chunk or an
        // earlier incremental pass.
        match self.kind {
            SpanKind::Chat => {
                if let Some(trace) = &self.trace_id {
                    if !resume.chat_trace_ids.contains(trace) {
                        resume.chat_trace_ids.push(trace.clone());
                        if resume.chat_trace_ids.len() > CHAT_TRACE_ID_CAP {
                            let overflow = resume.chat_trace_ids.len() - CHAT_TRACE_ID_CAP;
                            resume.chat_trace_ids.drain(..overflow);
                        }
                    }
                }
            }
            SpanKind::AgentSummary => {
                if self.trace_id.as_ref().is_some_and(|t| {
                    chunk_chat_traces.contains(t) || resume.chat_trace_ids.contains(t)
                }) {
                    return None;
                }
            }
        }

        let session_id = self
            .session_id
            .or_else(|| ctx.and_then(|c| c.session_id.clone()))
            .or_else(|| self.trace_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let model = self
            .model
            .or_else(|| ctx.and_then(|c| c.model.clone()))
            .unwrap_or_else(|| "unknown".to_string());
        // spanId is the stable per-span identity; the ledger's
        // (source, session_id, message_id) PRIMARY KEY with INSERT OR
        // IGNORE makes re-parse idempotent.
        let message_id = self
            .span_id
            .or(self.response_id)
            .unwrap_or_else(|| format!("line-{}", self.line_offset));

        let turn_index = resume
            .session_turn_counts
            .entry(session_id.clone())
            .or_insert(0);
        let this_turn = *turn_index;
        *turn_index += 1;

        let coverage = Coverage {
            has_input_tokens: true,
            has_output_tokens: true,
            has_reasoning_tokens: true,
            has_cache_read_tokens: true,
            has_cache_create_tokens: true,
            ..Coverage::EMPTY
        };

        Some(TurnRecord {
            v: 1,
            source: SourceKind::CopilotCli,
            session_id,
            session_path: session_path.map(str::to_string),
            message_id,
            turn_index: this_turn,
            ts: format_iso_ms(self.ts_ms),
            model,
            project: None,
            project_key: None,
            usage: self.usage,
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: None,
            stop_reason: self.stop_reason,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: Some(Fidelity::new(UsageGranularity::PerMessage, coverage)),
        })
    }
}

#[cfg(test)]
mod tests;
