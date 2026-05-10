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
    Ledger, LedgerOpenOptions, SessionListEntry, SessionsListOptions, SessionsListResult,
};
use serde_json::{json, Value};

use crate::cli::{GlobalArgs, SessionsArgs, SessionsListArgs, SessionsSubcommand};
use crate::render::error::report_error;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd, render_table};
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;

const DEFAULT_SINCE: &str = "7d";
const PROJECT_DISPLAY_WIDTH: usize = 56;

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
        emit_human(&result, &since, args.grep.as_deref());
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
    let _ = render_json(&payload);
}

fn emit_human(result: &SessionsListResult, since: &str, grep: Option<&str>) {
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

    let rows = session_table_rows(&result.sessions);
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

fn session_table_rows(sessions: &[SessionListEntry]) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = vec![vec![
        "session".into(),
        "project".into(),
        "last seen".into(),
        "turns".into(),
        "cost".into(),
    ]];
    for entry in sessions {
        rows.push(vec![
            entry.session_id.clone(),
            display_project(entry),
            format_last_seen(&entry.last_seen),
            format_uint(entry.turn_count),
            format_usd(entry.total_cost_usd),
        ]);
    }
    rows
}

fn display_project(entry: &SessionListEntry) -> String {
    entry
        .project
        .as_deref()
        .map(truncate_path_start)
        .unwrap_or_else(|| "—".to_string())
}

fn truncate_path_start(path: &str) -> String {
    let char_count = path.chars().count();
    if char_count <= PROJECT_DISPLAY_WIDTH {
        return path.to_string();
    }

    let suffix_budget = PROJECT_DISPLAY_WIDTH.saturating_sub(1);
    let skip = char_count.saturating_sub(suffix_budget);
    let raw_suffix: String = path.chars().skip(skip).collect();

    let suffix = raw_suffix
        .char_indices()
        .find(|(_, c)| *c == '/' || *c == '\\')
        .and_then(|(idx, _)| raw_suffix.get(idx..))
        .filter(|s| s.chars().count() >= suffix_budget / 2)
        .unwrap_or(raw_suffix.as_str());

    format!("…{suffix}")
}

fn format_last_seen(ts: &str) -> String {
    let Some((year, month, day, hour, minute)) = parse_iso_minute(ts) else {
        return ts.to_string();
    };
    let Some(month_name) = month_name(month) else {
        return ts.to_string();
    };
    let suffix = if hour < 12 { "am" } else { "pm" };
    let hour12 = match hour % 12 {
        0 => 12,
        n => n,
    };
    format!("{month_name} {day}, {year} - {hour12}:{minute:02}{suffix}")
}

fn parse_iso_minute(ts: &str) -> Option<(u32, u32, u32, u32, u32)> {
    let bytes = ts.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T') | Some(b' '))
        || bytes.get(13) != Some(&b':')
    {
        return None;
    }

    let year = parse_u32(ts, 0, 4)?;
    let month = parse_u32(ts, 5, 7)?;
    let day = parse_u32(ts, 8, 10)?;
    let hour = parse_u32(ts, 11, 13)?;
    let minute = parse_u32(ts, 14, 16)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    Some((year, month, day, hour, minute))
}

fn parse_u32(s: &str, start: usize, end: usize) -> Option<u32> {
    s.get(start..end)?.bytes().try_fold(0_u32, |acc, b| {
        if b.is_ascii_digit() {
            Some(acc * 10 + u32::from(b - b'0'))
        } else {
            None
        }
    })
}

fn month_name(month: u32) -> Option<&'static str> {
    match month {
        1 => Some("January"),
        2 => Some("February"),
        3 => Some("March"),
        4 => Some("April"),
        5 => Some("May"),
        6 => Some("June"),
        7 => Some("July"),
        8 => Some("August"),
        9 => Some("September"),
        10 => Some("October"),
        11 => Some("November"),
        12 => Some("December"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn session_table_rows_keep_full_session_id_and_one_human_date_column() {
        let long_id = "abcdef1234567890abcdef1234567890abcdef";
        let entry = SessionListEntry {
            session_id: long_id.into(),
            project: Some("/tmp/project".into()),
            started_at: "2026-05-08T11:00:00.000Z".into(),
            last_seen: "2026-05-08T12:23:00.000Z".into(),
            turn_count: 3,
            total_cost_usd: 0.0123,
            models: vec![],
        };

        let rows = session_table_rows(&[entry]);
        assert_eq!(
            rows[0],
            vec!["session", "project", "last seen", "turns", "cost"]
        );
        assert_eq!(rows[1][0], long_id);
        assert_eq!(rows[1][2], "May 8, 2026 - 12:23pm");
    }

    #[test]
    fn display_project_truncates_long_paths_from_the_start() {
        let path = "/Users/will/Projects/really/deep/workspace/with/a/very/long/project/root";
        let entry = SessionListEntry {
            session_id: "abc".into(),
            project: Some(path.into()),
            started_at: "2026-04-23T00:00:00.000Z".into(),
            last_seen: "2026-04-23T00:00:00.000Z".into(),
            turn_count: 1,
            total_cost_usd: 0.0,
            models: vec![],
        };

        let rendered = display_project(&entry);
        assert!(rendered.starts_with('…'));
        assert!(rendered.ends_with("/with/a/very/long/project/root"));
        assert!(rendered.chars().count() <= PROJECT_DISPLAY_WIDTH);
    }

    #[test]
    fn format_last_seen_handles_midnight_noon_and_invalid_input() {
        assert_eq!(
            format_last_seen("2026-05-08T00:03:00.000Z"),
            "May 8, 2026 - 12:03am"
        );
        assert_eq!(
            format_last_seen("2026-05-08T12:03:00.000Z"),
            "May 8, 2026 - 12:03pm"
        );
        assert_eq!(format_last_seen("not-a-date"), "not-a-date");
    }
}
