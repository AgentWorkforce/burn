//! `burn sessions list` — enumerate sessions in the ledger.
//!
//! Thin presenter over `relayburn_sdk::sessions_list`. Today the
//! `sessions` parent verb only carries `list`; the args struct keeps
//! room for follow-up nested verbs (`show`, `tag`, …) without churning
//! the dispatcher.
//!
//! ## Wiring
//!
//! 1. Open a [`LedgerHandle`] honoring the global `--ledger-path`.
//! 2. Lower CLI flags into [`SessionsListOptions`] (defaulting `since`
//!    to `7d` so a no-flag invocation is bounded for the common
//!    "what did I run recently" lookup).
//! 3. Render the typed `SessionsListResult` as JSON or a plain table.
//!
//! Cost is included — `cost_for_turn` is the same helper `summary` and
//! `session_cost` already use, so no new attribution code lives here.
//! Models with no pricing entry contribute zero, mirroring the rest of
//! the read-path verbs.

use relayburn_sdk::{
    Ledger, LedgerHandle, LedgerOpenOptions, SessionListEntry, SessionsListOptions,
    SessionsListResult,
};
use serde_json::{json, Value};

use crate::cli::{GlobalArgs, SessionsArgs, SessionsListArgs, SessionsSubcommand};
use crate::render::error::report_error;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd, render_table};
use crate::render::progress::TaskProgress;

const DEFAULT_SINCE: &str = "7d";
const SESSION_ID_DISPLAY_WIDTH: usize = 12;

pub fn run(globals: &GlobalArgs, args: SessionsArgs) -> i32 {
    match args.command {
        SessionsSubcommand::List(list_args) => run_list(globals, list_args),
    }
}

fn run_list(globals: &GlobalArgs, args: SessionsListArgs) -> i32 {
    match run_list_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_list_inner(globals: &GlobalArgs, args: SessionsListArgs) -> anyhow::Result<i32> {
    let progress = TaskProgress::new(globals, "sessions");

    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let handle = Ledger::open(opts).inspect_err(|_| {
        progress.finish_and_clear();
    })?;

    let since = args
        .since
        .clone()
        .unwrap_or_else(|| DEFAULT_SINCE.to_string());
    let sdk_opts = SessionsListOptions {
        since: Some(since.clone()),
        project: args.project.clone(),
        grep: args.grep.clone(),
        limit: args.limit,
        ledger_home: None,
    };

    progress.set_task("scanning sessions");
    let result = handle.sessions_list(sdk_opts).inspect_err(|_| {
        progress.finish_and_clear();
    })?;
    progress.finish_and_clear();

    if globals.json {
        emit_json(
            &result,
            &since,
            args.project.as_deref(),
            args.grep.as_deref(),
        );
    } else {
        emit_human(&result, &since, args.grep.as_deref(), &handle);
    }
    Ok(0)
}

fn emit_json(result: &SessionsListResult, since: &str, project: Option<&str>, grep: Option<&str>) {
    let mut filters = json!({ "since": since });
    if let Some(p) = project {
        filters
            .as_object_mut()
            .unwrap()
            .insert("project".into(), Value::String(p.to_string()));
    }
    if let Some(g) = grep {
        filters
            .as_object_mut()
            .unwrap()
            .insert("grep".into(), Value::String(g.to_string()));
    }

    let mut payload = json!({
        "filters": filters,
        "limit": result.limit,
        "truncated": result.truncated,
        "sessions": result.sessions,
    });
    coerce_whole_f64_to_int(&mut payload);
    let mut out = serde_json::to_string_pretty(&payload).unwrap_or_default();
    out.push('\n');
    print!("{}", out);
}

fn emit_human(
    result: &SessionsListResult,
    since: &str,
    grep: Option<&str>,
    _handle: &LedgerHandle,
) {
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());

    if result.sessions.is_empty() {
        lines.push(format!(
            "no sessions found (since {since}{}).",
            grep.map(|g| format!(", grep \"{g}\"")).unwrap_or_default(),
        ));
        let mut out = lines.join("\n");
        out.push('\n');
        print!("{}", out);
        return;
    }

    let mut rows: Vec<Vec<String>> = vec![vec![
        "session".into(),
        "project".into(),
        "started".into(),
        "last seen".into(),
        "turns".into(),
        "cost".into(),
    ]];
    for entry in &result.sessions {
        rows.push(vec![
            short_session_id(&entry.session_id),
            display_project(entry),
            entry.started_at.clone(),
            entry.last_seen.clone(),
            format_uint(entry.turn_count),
            format_usd(entry.total_cost_usd),
        ]);
    }
    lines.push(render_table(&rows));
    lines.push(String::new());
    lines.push(format!(
        "showing {} session{} (since {since}, limit {}){}",
        format_uint(result.sessions.len() as u64),
        if result.sessions.len() == 1 { "" } else { "s" },
        format_uint(result.limit),
        if result.truncated {
            "; more available — re-run with --limit to widen".to_string()
        } else {
            String::new()
        },
    ));
    lines.push("(pass the full session id to `burn summary --session <id>` for details)".into());
    lines.push(String::new());
    print!("{}", lines.join("\n"));
}

/// Truncate the session id for human display. Wide enough to disambiguate
/// the on-disk layout (the per-harness session-id schemes all carry their
/// entropy in the first 12 chars) without dominating the row width. JSON
/// output keeps the full id so scripts pipe through unaffected.
fn short_session_id(id: &str) -> String {
    if id.chars().count() <= SESSION_ID_DISPLAY_WIDTH {
        return id.to_string();
    }
    let prefix: String = id.chars().take(SESSION_ID_DISPLAY_WIDTH).collect();
    format!("{prefix}…")
}

fn display_project(entry: &SessionListEntry) -> String {
    entry.project.clone().unwrap_or_else(|| "—".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_session_id_truncates_long_ids() {
        let id = "abcdef1234567890abcdef";
        assert_eq!(short_session_id(id), "abcdef123456…");
    }

    #[test]
    fn short_session_id_passes_short_ids_through() {
        let id = "sess-old";
        assert_eq!(short_session_id(id), "sess-old");
    }

    #[test]
    fn display_project_falls_back_to_dash_when_missing() {
        let entry = SessionListEntry {
            session_id: "abc".into(),
            project: None,
            started_at: "2026-04-23T00:00:00.000Z".into(),
            last_seen: "2026-04-23T00:00:00.000Z".into(),
            turn_count: 1,
            total_cost_usd: 0.0,
            models: vec![],
        };
        assert_eq!(display_project(&entry), "—");
    }
}
