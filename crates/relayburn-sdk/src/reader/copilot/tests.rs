//! Copilot CLI OTEL parser tests. Fixtures live at the repo root
//! (`tests/fixtures/copilot-cli/*.jsonl`) and model the span shapes Copilot
//! CLI's OTEL file exporter emits (see tokscale's `sessions/copilot.rs` test
//! module and the GitHub Docs Copilot CLI command reference).

use std::io::Write;
use std::path::PathBuf;

use tempfile::tempdir;

use super::*;
use crate::reader::types::{FidelityClass, UsageGranularity};
use crate::util::time::format_iso_ms;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/relayburn-sdk/ -> repo root
    p.pop();
    p.pop();
    p.push("tests/fixtures/copilot-cli");
    p.push(name);
    p
}

#[test]
fn chat_spans_fixture() {
    let result = parse_copilot_file(&fixture("chat-spans.jsonl")).unwrap();
    let turns = &result.turns;

    // 9 usage-relevant lines in, 5 turns out: the invoke_agent summary for
    // trace-1 is suppressed (chat spans present), the zero-token chat span
    // and the execute_tool span carry no usage, the metric line and the
    // malformed line are skipped.
    assert_eq!(turns.len(), 5);
    assert!(turns
        .iter()
        .all(|t| matches!(t.source, SourceKind::CopilotCli)));

    let first = &turns[0];
    assert_eq!(first.session_id, "conv-1");
    assert_eq!(first.message_id, "span-1");
    assert_eq!(first.turn_index, 0);
    assert_eq!(first.model, "claude-sonnet-4.6");
    assert_eq!(first.ts, format_iso_ms(1_775_934_260_133));
    // input is exclusive of cache reads: 19452 - 123.
    assert_eq!(first.usage.input, 19_329);
    assert_eq!(first.usage.output, 281);
    assert_eq!(first.usage.cache_read, 123);
    assert_eq!(first.usage.cache_create_5m, 21_881);
    assert_eq!(first.usage.reasoning, 128);
    assert_eq!(first.stop_reason, Some(StopReason::EndTurn));

    let second = &turns[1];
    assert_eq!(second.session_id, "conv-1");
    assert_eq!(second.turn_index, 1);
    assert_eq!(second.usage.input, 300); // 20100 - 19800
    assert_eq!(second.usage.cache_read, 19_800);
    assert_eq!(second.stop_reason, None);

    // invoke_agent for trace-2: no chat spans in that trace, so the
    // aggregate is the only usage signal and must be kept. Model falls back
    // to gen_ai.request.model; cache attrs use the CLI's underscored
    // spelling.
    let summary = &turns[2];
    assert_eq!(summary.message_id, "invoke-2");
    assert_eq!(summary.session_id, "conv-2");
    assert_eq!(summary.turn_index, 0);
    assert_eq!(summary.model, "gpt-5.4-mini");
    assert_eq!(summary.usage.input, 200); // 4200 - 4000
    assert_eq!(summary.usage.output, 310);
    assert_eq!(summary.usage.cache_read, 4_000);
    assert_eq!(summary.usage.cache_create_5m, 150);

    // String-coerced token counts; no session attr → trace id is the
    // session fallback.
    let coerced = &turns[3];
    assert_eq!(coerced.session_id, "trace-3");
    assert_eq!(coerced.usage.input, 7);
    assert_eq!(coerced.usage.output, 9);

    // VS-Code-style record: no `type`, span identity nested under
    // `spanContext`, timestamp under `hrTime`.
    let vsc = &turns[4];
    assert_eq!(vsc.message_id, "spanctx-1");
    assert_eq!(vsc.session_id, "trace-6");
    assert_eq!(vsc.model, "gpt-5.4");
    assert_eq!(vsc.ts, format_iso_ms(1_775_934_700_250));

    // Every turn declares usage-only fidelity at per-message granularity,
    // with every token bucket covered.
    for t in turns {
        let fidelity = t.fidelity.as_ref().expect("fidelity set");
        assert_eq!(fidelity.granularity, UsageGranularity::PerMessage);
        assert_eq!(fidelity.class, FidelityClass::UsageOnly);
        let c = &fidelity.coverage;
        assert!(c.has_input_tokens && c.has_output_tokens && c.has_reasoning_tokens);
        assert!(c.has_cache_read_tokens && c.has_cache_create_tokens);
        assert!(t.tool_calls.is_empty());
    }
}

#[test]
fn incremental_resume_and_partial_tail() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    let span_a = br#"{"type":"span","traceId":"t-a","spanId":"s-a","name":"chat m","startTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"conv-a","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;
    let span_b = br#"{"type":"span","traceId":"t-b","spanId":"s-b","name":"chat m","startTime":[1775934270,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"conv-a","gen_ai.usage.input_tokens":20,"gen_ai.usage.output_tokens":4}}"#;

    // First pass: one complete span plus a partial second line still being
    // flushed. The partial tail must not be consumed.
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(span_a).unwrap();
    f.write_all(b"\n").unwrap();
    f.write_all(&span_b[..40]).unwrap(); // partial, no newline
    drop(f);

    let first =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 1);
    assert_eq!(first.turns[0].message_id, "s-a");
    assert_eq!(first.turns[0].turn_index, 0);
    assert_eq!(first.end_offset, (span_a.len() + 1) as u64);

    // Complete the second span and re-ingest from the cursor.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&span_b[40..]).unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);

    let second = parse_copilot_otel_incremental(
        &path,
        &ParseCopilotIncrementalOptions {
            start_offset: Some(first.end_offset),
            resume: Some(first.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.turns.len(), 1);
    assert_eq!(second.turns[0].message_id, "s-b");
    // turn_index continues per session across incremental passes.
    assert_eq!(second.turns[0].turn_index, 1);
}

#[test]
fn invoke_agent_suppressed_across_incremental_passes() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    let chat = br#"{"type":"span","traceId":"t-x","spanId":"s-x","name":"chat m","startTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"conv-x","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;
    let summary = br#"{"type":"span","traceId":"t-x","spanId":"i-x","name":"invoke_agent","startTime":[1775934259,0],"attributes":{"gen_ai.operation.name":"invoke_agent","gen_ai.conversation.id":"conv-x","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;

    std::fs::write(&path, [chat.as_slice(), b"\n"].concat()).unwrap();
    let first =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 1);

    // The summary span flushes in a later pass; the carried chat_trace_ids
    // must still suppress it.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(summary).unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);

    let second = parse_copilot_otel_incremental(
        &path,
        &ParseCopilotIncrementalOptions {
            start_offset: Some(first.end_offset),
            resume: Some(first.resume),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(second.turns.is_empty());
}

#[test]
fn invoke_agent_before_chat_in_same_chunk_is_suppressed() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    // Exporter flush order isn't contractual: the aggregate summary can
    // precede its trace's chat spans in the same chunk. Suppression must
    // not depend on record order, or the trace is double counted.
    let summary = br#"{"type":"span","traceId":"t-ord","spanId":"i-ord","name":"invoke_agent","startTime":[1775934259,0],"attributes":{"gen_ai.operation.name":"invoke_agent","gen_ai.conversation.id":"conv-ord","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;
    let chat = br#"{"type":"span","traceId":"t-ord","spanId":"s-ord","name":"chat m","startTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"conv-ord","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;

    std::fs::write(
        &path,
        [summary.as_slice(), b"\n", chat.as_slice(), b"\n"].concat(),
    )
    .unwrap();
    let parsed =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(parsed.turns.len(), 1);
    assert_eq!(parsed.turns[0].message_id, "s-ord");
}

#[test]
fn chat_span_name_provides_model_fallback() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    // No gen_ai.{request,response}.model attributes: the `chat <model>`
    // span name is the only model signal and must beat "unknown".
    let span = br#"{"type":"span","traceId":"t-nm","spanId":"s-nm","name":"chat claude-sonnet-4.6","startTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"conv-nm","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;
    std::fs::write(&path, [span.as_slice(), b"\n"].concat()).unwrap();
    let parsed =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(parsed.turns.len(), 1);
    assert_eq!(parsed.turns[0].model, "claude-sonnet-4.6");
}

#[test]
fn id_less_spans_get_content_stable_message_ids() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    // Spans with neither spanId nor gen_ai.response.id fall back to a
    // digest of the record bytes. A chunk-local index or a byte offset
    // would restart at zero on the next pass or file generation and
    // collide in the ledger key; the digest keeps distinct spans distinct
    // and a re-read span identical.
    let line = |trace: &str| {
        format!(
            "{{\"type\":\"span\",\"traceId\":\"{trace}\",\"name\":\"chat m\",\"startTime\":[1775934260,0],\"attributes\":{{\"gen_ai.operation.name\":\"chat\",\"gen_ai.conversation.id\":\"conv-off\",\"gen_ai.usage.input_tokens\":10,\"gen_ai.usage.output_tokens\":2}}}}"
        )
    };
    let line_a = line("t-off-a");
    let line_b = line("t-off-b");
    std::fs::write(&path, format!("{line_a}\n")).unwrap();
    let first =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 1);
    let id_a = first.turns[0].message_id.clone();
    assert_eq!(id_a, format!("line-{}", line_digest(line_a.as_bytes())));
    assert_eq!(id_a.len(), "line-".len() + 16);

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{line_b}").unwrap();
    drop(f);

    let second = parse_copilot_otel_incremental(
        &path,
        &ParseCopilotIncrementalOptions {
            start_offset: Some(first.end_offset),
            resume: Some(first.resume),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.turns.len(), 1);
    assert_ne!(id_a, second.turns[0].message_id);

    // A rotated file generation starting with span B at offset 0 keeps
    // B's id rather than reusing A's.
    let rotated = tmp.path().join("rotated.jsonl");
    std::fs::write(&rotated, format!("{line_b}\n")).unwrap();
    let third =
        parse_copilot_otel_incremental(&rotated, &ParseCopilotIncrementalOptions::default())
            .unwrap();
    assert_eq!(third.turns[0].message_id, second.turns[0].message_id);
}

#[test]
fn empty_session_attr_falls_through_to_next_priority_key() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    // A blank gen_ai.conversation.id must not hide copilot_chat.session_id;
    // otherwise the turn falls back to the trace id and splits the session.
    let span = br#"{"type":"span","traceId":"t-es","spanId":"s-es","name":"chat m","startTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"  ","copilot_chat.session_id":"conv-es","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":2}}"#;
    std::fs::write(&path, [span.as_slice(), b"\n"].concat()).unwrap();
    let parsed =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(parsed.turns.len(), 1);
    assert_eq!(parsed.turns[0].session_id, "conv-es");
}

#[test]
fn persisted_spans_are_skipped_without_consuming_turn_indexes() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");

    // A rotation replays s-1 and s-2 (already in the ledger) ahead of the
    // new s-3. The replays must not advance the session counter, so s-3
    // takes index 2 rather than 4.
    let span = |id: &str| {
        format!(
            "{{\"type\":\"span\",\"traceId\":\"t-{id}\",\"spanId\":\"{id}\",\"name\":\"chat m\",\"startTime\":[1775934260,0],\"attributes\":{{\"gen_ai.operation.name\":\"chat\",\"gen_ai.conversation.id\":\"conv-p\",\"gen_ai.usage.input_tokens\":10,\"gen_ai.usage.output_tokens\":2}}}}\n"
        )
    };
    std::fs::write(&path, [span("s-1"), span("s-2"), span("s-3")].concat()).unwrap();
    let persisted: HashSet<(String, String)> = ["s-1", "s-2"]
        .iter()
        .map(|id| ("conv-p".to_string(), id.to_string()))
        .collect();
    let mut resume = CopilotResumeState::default();
    resume.session_turn_counts.insert("conv-p".into(), 2);
    let parsed = parse_copilot_otel_incremental(
        &path,
        &ParseCopilotIncrementalOptions {
            resume: Some(resume),
            persisted,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(parsed.turns.len(), 1);
    assert_eq!(parsed.turns[0].message_id, "s-3");
    assert_eq!(parsed.turns[0].turn_index, 2);
    assert_eq!(parsed.resume.session_turn_counts.get("conv-p"), Some(&3));
    // Replayed chat spans still register their traces for summary
    // suppression.
    assert!(parsed.resume.chat_trace_ids.contains(&"t-s-1".to_string()));
}

#[test]
fn empty_and_missing_files() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, b"").unwrap();
    let parsed =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert!(parsed.turns.is_empty());
    assert_eq!(parsed.end_offset, 0);

    let missing = tmp.path().join("nope.jsonl");
    assert!(
        parse_copilot_otel_incremental(&missing, &ParseCopilotIncrementalOptions::default())
            .is_err()
    );
}

#[test]
fn chat_trace_ids_stay_bounded_and_evict_oldest() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");
    let mut body = String::new();
    for i in 0..=CHAT_TRACE_ID_CAP {
        body.push_str(&format!(
            "{{\"type\":\"span\",\"traceId\":\"t-cap-{i}\",\"spanId\":\"s-cap-{i}\",\"name\":\"chat m\",\"startTime\":[1775934260,{i}],\"attributes\":{{\"gen_ai.operation.name\":\"chat\",\"gen_ai.conversation.id\":\"conv-cap\",\"gen_ai.usage.input_tokens\":10,\"gen_ai.usage.output_tokens\":2}}}}\n"
        ));
    }
    std::fs::write(&path, body).unwrap();
    let parsed =
        parse_copilot_otel_incremental(&path, &ParseCopilotIncrementalOptions::default()).unwrap();
    assert_eq!(parsed.turns.len(), CHAT_TRACE_ID_CAP + 1);
    let ids = &parsed.resume.chat_trace_ids;
    assert_eq!(ids.len(), CHAT_TRACE_ID_CAP);
    assert_eq!(ids.first().map(String::as_str), Some("t-cap-1"));
    assert_eq!(
        ids.last().map(String::as_str),
        Some(format!("t-cap-{CHAT_TRACE_ID_CAP}").as_str())
    );
}

/// Parse `lines` (one JSON record each) as a fresh export file.
fn parse_lines(lines: &[String]) -> ParseCopilotIncrementalResult {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("copilot.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    parse_copilot_otel_incremental(
        &path,
        &ParseCopilotIncrementalOptions {
            fallback_ts_ms: Some(42),
            ..Default::default()
        },
    )
    .unwrap()
}

/// A record with `fields` spliced into its top level and `attrs` into its
/// attributes (both JSON object bodies without braces).
fn record(fields: &str, attrs: &str) -> String {
    let sep = if attrs.is_empty() { "" } else { "," };
    format!(
        "{{{fields},\"attributes\":{{\"gen_ai.conversation.id\":\"conv-shape\",\"gen_ai.usage.input_tokens\":10{sep}{attrs}}}}}"
    )
}

#[test]
fn only_span_records_become_turns() {
    // A non-span record with chat attributes and usage is ignored.
    let log = record(
        r#""type":"log","traceId":"t-l","spanId":"s-l","name":"chat m""#,
        r#""gen_ai.operation.name":"chat""#,
    );
    // Without `type`, span-ness needs both a name and a span id.
    let nameless = record(
        r#""traceId":"t-n","spanId":"s-n""#,
        r#""gen_ai.operation.name":"chat""#,
    );
    let idless = record(
        r#""traceId":"t-i","name":"chat m""#,
        r#""gen_ai.operation.name":"chat""#,
    );
    let vscode = record(
        r#""name":"chat m","spanContext":{"traceId":"t-v","spanId":"s-v"}"#,
        r#""gen_ai.operation.name":"chat""#,
    );
    let parsed = parse_lines(&[log, nameless, idless, vscode]);
    let ids: Vec<&str> = parsed.turns.iter().map(|t| t.message_id.as_str()).collect();
    assert_eq!(ids, ["s-v"]);
}

#[test]
fn spans_classify_by_operation_or_by_name() {
    let lines = [
        // Chat by operation attribute alone, and by `chat <model>` name alone.
        record(
            r#""type":"span","traceId":"t-1","spanId":"op-chat","name":"llm call""#,
            r#""gen_ai.operation.name":"chat""#,
        ),
        record(
            r#""type":"span","traceId":"t-2","spanId":"name-chat","name":"chat m""#,
            "",
        ),
        // Agent summaries by operation, by `invoke_agent <name>`, and by the
        // bare `invoke_agent` name.
        record(
            r#""type":"span","traceId":"t-3","spanId":"op-agent","name":"agent run""#,
            r#""gen_ai.operation.name":"invoke_agent""#,
        ),
        record(
            r#""type":"span","traceId":"t-4","spanId":"named-agent","name":"invoke_agent burn""#,
            "",
        ),
        record(
            r#""type":"span","traceId":"t-5","spanId":"bare-agent","name":"invoke_agent""#,
            "",
        ),
        // Neither: a tool span with usage stays out.
        record(
            r#""type":"span","traceId":"t-6","spanId":"tool","name":"execute_tool rg""#,
            r#""gen_ai.operation.name":"execute_tool""#,
        ),
    ];
    let parsed = parse_lines(&lines);
    let ids: Vec<&str> = parsed.turns.iter().map(|t| t.message_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "op-chat",
            "name-chat",
            "op-agent",
            "named-agent",
            "bare-agent"
        ]
    );
}

#[test]
fn sentinel_and_blank_span_ids_are_absent() {
    let lines = [
        record(
            r#""type":"span","traceId":"t-z","spanId":"0000000000000000","name":"chat m""#,
            "",
        ),
        record(
            r#""type":"span","traceId":"t-d","spanId":"00-00","name":"chat m""#,
            r#""gen_ai.usage.output_tokens":1"#,
        ),
        record(
            r#""type":"span","traceId":"t-b","spanId":"  ","name":"chat m""#,
            r#""gen_ai.response.id":" ","gen_ai.usage.output_tokens":2"#,
        ),
        record(
            r#""type":"span","traceId":"t-ok","spanId":"abc123","name":"chat m""#,
            "",
        ),
        record(
            r#""type":"span","traceId":"t-r","name":"chat m""#,
            r#""gen_ai.response.id":"resp9""#,
        ),
    ];
    let parsed = parse_lines(&lines);
    let ids: Vec<&str> = parsed.turns.iter().map(|t| t.message_id.as_str()).collect();
    assert_eq!(ids.len(), 5);
    for id in &ids[..3] {
        assert!(
            id.starts_with("line-"),
            "{id} should use the digest fallback"
        );
    }
    assert_eq!(&ids[3..], ["abc123", "resp9"]);
}

#[test]
fn timestamps_accept_pairs_iso_and_scalar_units() {
    let at = |start: &str, id: &str| {
        record(
            &format!(
                r#""type":"span","traceId":"t-{id}","spanId":"{id}","name":"chat m","startTime":{start}"#
            ),
            "",
        )
    };
    let parsed = parse_lines(&[
        at("[1775934260,133000000]", "pair"),
        at(r#""2026-04-11T19:04:20.133Z""#, "iso"),
        at("1775934260133000000", "ns"),
        at("1775934260133000", "us"),
        at("1775934260133", "ms"),
        at("1775934260", "s"),
        at("[]", "empty"),
    ]);
    let ts: Vec<&str> = parsed.turns.iter().map(|t| t.ts.as_str()).collect();
    let expected = format_iso_ms(1_775_934_260_133);
    assert_eq!(&ts[..5], [expected.as_str(); 5]);
    assert_eq!(ts[5], format_iso_ms(1_775_934_260_000));
    // An empty pair is no timestamp: the file-mtime fallback applies.
    assert_eq!(ts[6], format_iso_ms(42));
}

#[test]
fn usage_spans_need_any_nonzero_bucket() {
    let span = |id: &str, attrs: &str| {
        format!(
            "{{\"type\":\"span\",\"traceId\":\"t-{id}\",\"spanId\":\"{id}\",\"name\":\"chat m\",\"attributes\":{{\"gen_ai.conversation.id\":\"conv-u\",{attrs}}}}}"
        )
    };
    let parsed = parse_lines(&[
        span("in", r#""gen_ai.usage.input_tokens":5"#),
        span("out", r#""gen_ai.usage.output_tokens":5"#),
        span("cr", r#""gen_ai.usage.cache_read.input_tokens":5"#),
        span("cw", r#""gen_ai.usage.cache_write.input_tokens":5"#),
        span("rs", r#""gen_ai.usage.reasoning.output_tokens":5"#),
        span(
            "zero",
            r#""gen_ai.usage.input_tokens":0,"gen_ai.usage.output_tokens":0"#,
        ),
    ]);
    let ids: Vec<&str> = parsed.turns.iter().map(|t| t.message_id.as_str()).collect();
    assert_eq!(ids, ["in", "out", "cr", "cw", "rs"]);
}

#[test]
fn finish_reason_accepts_a_bare_string() {
    let parsed = parse_lines(&[record(
        r#""type":"span","traceId":"t-f","spanId":"s-f","name":"chat m""#,
        r#""gen_ai.response.finish_reasons":"stop""#,
    )]);
    assert_eq!(parsed.turns[0].stop_reason, Some(StopReason::EndTurn));
}

#[test]
fn sibling_spans_supply_missing_model_and_session() {
    // The usage span names neither its model nor its session; a sibling in
    // the same trace carries both.
    let usage = r#"{"type":"span","traceId":"t-ctx","spanId":"s-ctx","name":"llm call","attributes":{"gen_ai.operation.name":"chat","gen_ai.usage.input_tokens":10}}"#;
    let sibling = r#"{"type":"span","traceId":"t-ctx","spanId":"s-tool","name":"execute_tool rg","attributes":{"gen_ai.request.model":"gpt-5.4","copilot_chat.session_id":"conv-ctx"}}"#;
    let parsed = parse_lines(&[usage.to_string(), sibling.to_string()]);
    assert_eq!(parsed.turns.len(), 1);
    assert_eq!(parsed.turns[0].model, "gpt-5.4");
    assert_eq!(parsed.turns[0].session_id, "conv-ctx");
}
