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

    // Every turn declares usage-only fidelity at per-message granularity.
    for t in turns {
        let fidelity = t.fidelity.as_ref().expect("fidelity set");
        assert_eq!(fidelity.granularity, UsageGranularity::PerMessage);
        assert_eq!(fidelity.class, FidelityClass::UsageOnly);
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
