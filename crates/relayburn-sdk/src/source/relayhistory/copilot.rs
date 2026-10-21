//! copilot-specific refinements of the shared evidence mapping.
//!
//! Copilot CLI writes one `assistant.message` per model call, so a burn turn
//! is one assistant message. The only per-call counter it persists is
//! `outputTokens`; input and cache usage exist only as the per-model running
//! totals each `session.shutdown` records. Those totals are billed here as
//! one per-session-aggregate turn per shutdown and model: what the totals
//! grew by since the previous shutdown, less the output the session's
//! messages already billed.

use std::collections::{BTreeSet, HashMap};

use ai_hist::{Marker, Message};
use serde_json::Value;

use super::Context;
use crate::reader::hash::content_hash;
use crate::reader::resolve_project;
use crate::reader::types::{
    Coverage, Fidelity, Subagent, ToolCall, TurnRecord, Usage, UsageGranularity,
};
use crate::source::usage::{copilot_totals, usage_from_raw};
use crate::source::SessionRecords;
use crate::util::time::format_iso_ms;

/// Derive what the shared mapping cannot for copilot sessions.
pub(super) fn refine(ctx: &Context<'_>, records: &mut SessionRecords) {
    for turn in &mut records.turns {
        refine_turn(ctx, turn);
    }
    records.turns.extend(shutdown_turns(ctx));
    // A shutdown's turn sits where the journal wrote it, so a session that
    // resumes appends turns after it and earlier indices never move.
    records.turns.sort_by(|a, b| a.ts.cmp(&b.ts));
    for (index, turn) in records.turns.iter_mut().enumerate() {
        turn.turn_index = index as u64;
    }
}

/// Copilot's tool vocabulary for targets, edit hashes and touched files;
/// subagent turns flagged; then the activity classification over them.
fn refine_turn(ctx: &Context<'_>, turn: &mut TurnRecord) {
    let message_ids: Vec<&str> = ctx
        .units
        .iter()
        .filter(|u| u.id == turn.message_id)
        .flat_map(|u| &u.request.message_ids)
        .map(String::as_str)
        .collect();
    let mut files = BTreeSet::new();
    for call in &mut turn.tool_calls {
        // Copilot reuses a tool call id across turns: a call is the one its
        // own messages made.
        let input = ctx
            .ev
            .tool_calls
            .iter()
            .find(|c| {
                c.tool_use_id == call.id
                    && c.message_id
                        .as_deref()
                        .is_some_and(|m| message_ids.contains(&m))
            })
            .and_then(|c| c.args.clone())
            .unwrap_or(Value::Null);
        apply_tool(call, &input);
        if matches!(
            call.name.as_str(),
            "view" | "read" | "edit" | "create" | "write"
        ) {
            if let Some(target) = &call.target {
                files.insert(target.clone());
            }
        }
    }
    turn.files_touched = (!files.is_empty()).then(|| files.into_iter().collect());
    let messages: Vec<&Message> = message_ids
        .iter()
        .filter_map(|id| ctx.by_id.get(id).copied())
        .collect();
    if messages.iter().any(|m| m.is_sidechain == Some(true)) {
        turn.subagent = Some(Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: None,
            parent_agent_id: None,
            subagent_type: None,
            description: None,
        });
    }
    ctx.classify(turn, &messages);
}

fn apply_tool(call: &mut ToolCall, input: &Value) {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    // `view`/`edit`/`create` name a `path`, `web_fetch` a `url`, `bash` a
    // `command`; the rest name what they act on differently.
    call.target = match call.name.as_str() {
        "grep" | "glob" => s("pattern"),
        "task" => s("agent_type").or_else(|| s("description")),
        _ => s("path").or_else(|| s("url")).or_else(|| s("command")),
    };
    let hash = |k: &str| input.get(k).and_then(Value::as_str).map(content_hash);
    match call.name.as_str() {
        "edit" => {
            call.edit_pre_hash = hash("old_str");
            call.edit_post_hash = hash("new_str");
        }
        "create" => call.edit_post_hash = hash("file_text"),
        _ => {}
    }
}

/// One `session.shutdown`: when it was written and each model's totals.
struct Shutdown<'a> {
    marker: &'a Marker,
    ts_ms: i64,
    totals: Vec<(String, Usage)>,
}

fn shutdowns<'a>(ctx: &Context<'a>) -> Vec<Shutdown<'a>> {
    let mut out: Vec<Shutdown> = ctx
        .ev
        .markers
        .iter()
        .filter(|m| m.kind == "session.shutdown")
        .filter_map(|marker| {
            let metrics = marker.payload.as_ref()?.pointer("/data/modelMetrics")?;
            let totals = metrics
                .as_object()?
                .iter()
                .filter_map(|(model, metric)| {
                    Some((model.clone(), copilot_totals(&metric["usage"])?))
                })
                .collect();
            Some(Shutdown {
                marker,
                ts_ms: marker.ts_ms?,
                totals,
            })
        })
        .collect();
    out.sort_by_key(|s| s.ts_ms);
    out
}

/// Output the session's messages billed per model, in `(after, through]`.
/// Only assistant messages carry Copilot's `outputTokens`.
fn message_output(ctx: &Context<'_>, after: i64, through: i64) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for message in &ctx.ev.messages {
        if message.ts_ms <= after || message.ts_ms > through {
            continue;
        }
        let (usage, coverage) = usage_from_raw(ctx.source, message.raw_usage());
        if let (true, Some(model)) = (coverage.has_output_tokens, message.model.as_ref()) {
            *out.entry(model.clone()).or_default() += usage.output;
        }
    }
    out
}

/// What a running total grew by. A total lower than the previous one
/// restarted, and counts whole.
fn growth(now: &Usage, before: Option<&Usage>) -> Usage {
    let Some(before) = before else {
        return now.clone();
    };
    let pairs = [
        (now.input, before.input),
        (now.output, before.output),
        (now.reasoning, before.reasoning),
        (now.cache_read, before.cache_read),
        (now.cache_create_5m, before.cache_create_5m),
    ];
    if pairs.iter().any(|(n, b)| n < b) {
        return now.clone();
    }
    Usage {
        input: now.input - before.input,
        output: now.output - before.output,
        reasoning: now.reasoning - before.reasoning,
        cache_read: now.cache_read - before.cache_read,
        cache_create_5m: now.cache_create_5m - before.cache_create_5m,
        cache_create_1h: 0,
    }
}

fn shutdown_turns(ctx: &Context<'_>) -> Vec<TurnRecord> {
    let mut turns = Vec::new();
    let mut previous: HashMap<String, Usage> = HashMap::new();
    let mut after = i64::MIN;
    for shutdown in shutdowns(ctx) {
        let billed = message_output(ctx, after, shutdown.ts_ms);
        for (model, totals) in &shutdown.totals {
            let mut usage = growth(totals, previous.get(model));
            usage.output = usage
                .output
                .saturating_sub(billed.get(model).copied().unwrap_or(0));
            if usage != Usage::default() {
                turns.push(aggregate_turn(ctx, &shutdown, model, usage));
            }
            previous.insert(model.to_string(), totals.clone());
        }
        after = shutdown.ts_ms;
    }
    turns
}

fn aggregate_turn(
    ctx: &Context<'_>,
    shutdown: &Shutdown<'_>,
    model: &str,
    usage: Usage,
) -> TurnRecord {
    let resolved = ctx.ev.session.cwd.as_deref().map(resolve_project);
    TurnRecord {
        v: 1,
        source: ctx.source,
        session_id: ctx.session_id().to_string(),
        session_path: ctx
            .ev
            .session
            .raw_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        message_id: format!("{}:{model}", shutdown.marker.marker_uid),
        turn_index: 0,
        ts: format_iso_ms(shutdown.ts_ms),
        model: model.to_string(),
        project: resolved.as_ref().map(|r| r.project.clone()),
        project_key: resolved.and_then(|r| r.project_key),
        usage,
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: Some(Fidelity::new(
            UsageGranularity::PerSessionAggregate,
            Coverage {
                has_input_tokens: true,
                has_output_tokens: true,
                has_reasoning_tokens: true,
                has_cache_read_tokens: true,
                has_cache_create_tokens: true,
                ..Coverage::default()
            },
        )),
        reasoning: None,
        // Running totals, not a model call.
        request_count: Some(0),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::reader::classifier::normalize_tool_name;

    fn usage(input: u64, output: u64, cache_read: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            ..Usage::default()
        }
    }

    /// Records for one Copilot journal, read through a throwaway store.
    fn records_for(journal: &str) -> SessionRecords {
        let home = tempfile::tempdir().unwrap();
        let sid = "boundary-session";
        let dir = home.path().join(".copilot/session-state").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), journal).unwrap();
        let mut options = ai_hist::StoreOptions::default();
        options.db_path = Some(home.path().join("ai-history.db"));
        options.roots = Some(ai_hist::ProviderRoots::from_home(
            home.path().to_path_buf(),
            home.path().join("opencode.db"),
        ));
        let store = ai_hist::SessionStore::open(options).unwrap();
        store.sync(Default::default()).unwrap();
        let evidence = store
            .session(
                &ai_hist::SessionRef::id(ai_hist::Source::Copilot, sid),
                ai_hist::SessionQuery::default(),
            )
            .unwrap()
            .unwrap();
        super::super::records_from_evidence(&evidence, &crate::source::Billing::settled())
    }

    #[test]
    fn a_message_written_with_its_shutdown_is_billed_once() {
        let at = "2026-09-18T21:30:05.000Z";
        let journal = [
            json!({"type":"session.start","data":{"version":1,"sessionId":"boundary-session","context":{"cwd":"/tmp/project"}},"id":"start","timestamp":"2026-09-18T21:30:00.000Z","parentId":null}),
            json!({"type":"assistant.message","data":{"messageId":"m1","model":"gpt-5.4","content":"done","outputTokens":5},"id":"a1","timestamp":at,"parentId":"start"}),
            json!({"type":"session.shutdown","data":{"modelMetrics":{"gpt-5.4":{"usage":{"inputTokens":100,"outputTokens":7,"cacheReadTokens":0,"cacheWriteTokens":0}}}},"id":"shut","timestamp":at,"parentId":"a1"}),
        ]
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<String>();
        let records = records_for(&journal);
        let output: u64 = records.turns.iter().map(|t| t.usage.output).sum();
        let input: u64 = records.turns.iter().map(|t| t.usage.input).sum();
        assert_eq!((input, output), (100, 7));
    }

    #[test]
    fn running_totals_bill_their_growth_and_a_restart_whole() {
        let first = usage(100, 10, 50);
        assert_eq!(growth(&first, None), first);
        assert_eq!(growth(&usage(160, 15, 90), Some(&first)), usage(60, 5, 40));
        let restarted = usage(20, 2, 0);
        assert_eq!(growth(&restarted, Some(&first)), restarted);
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "id".into(),
            name: name.into(),
            target: None,
            args_hash: String::new(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    #[test]
    fn copilot_tools_name_their_targets_and_edit_hashes() {
        let mut edit = call("edit");
        apply_tool(
            &mut edit,
            &json!({"path": "/src/lib.rs", "old_str": "a", "new_str": "b"}),
        );
        assert_eq!(edit.target.as_deref(), Some("/src/lib.rs"));
        assert_eq!(edit.edit_pre_hash, Some(content_hash("a")));
        assert_eq!(edit.edit_post_hash, Some(content_hash("b")));
        let mut create = call("create");
        apply_tool(&mut create, &json!({"path": "/new.rs", "file_text": "c"}));
        assert_eq!(create.edit_post_hash, Some(content_hash("c")));
        for (name, input, target) in [
            ("bash", json!({"command": "ls"}), "ls"),
            ("grep", json!({"pattern": "p"}), "p"),
            ("web_fetch", json!({"url": "u"}), "u"),
            (
                "task",
                json!({"agent_type": "explore", "description": "d"}),
                "explore",
            ),
        ] {
            let mut c = call(name);
            apply_tool(&mut c, &input);
            assert_eq!(c.target.as_deref(), Some(target), "{name}");
        }
        for (name, canonical) in [
            ("view", "Read"),
            ("create", "Write"),
            ("edit", "Edit"),
            ("bash", "Bash"),
            ("web_fetch", "WebFetch"),
            ("read_agent", "Task"),
        ] {
            assert_eq!(normalize_tool_name(name), canonical, "{name}");
        }
    }
}
