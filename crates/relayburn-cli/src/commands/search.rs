//! `burn search <query>` — thin CLI presenter over the SDK FTS5 verb.

use anyhow::bail;
use relayburn_sdk::{
    is_valid_session_id, Ledger, LedgerOpenOptions, SearchHit, SearchQueryOptions, SearchResult,
    DEFAULT_SEARCH_LIMIT,
};
use serde_json::json;

use crate::cli::{GlobalArgs, SearchArgs};
use crate::render::error::report_error;
use crate::render::format::{coerce_whole_f64_to_int, render_table};
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;
use crate::render::ux;

pub fn run(globals: &GlobalArgs, args: SearchArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: SearchArgs) -> anyhow::Result<i32> {
    if args.query.trim().is_empty() {
        bail!("burn search: query must not be empty");
    }
    if let Some(session) = args.session.as_deref() {
        if !is_valid_session_id(session) {
            bail!("burn search: invalid session id `{session}`");
        }
    }

    let requested_limit = args.limit.map(|value| value.get());
    let applied_limit = requested_limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let progress = TaskProgress::new(globals, "search");
    progress.set_task("opening content store");
    let handle = Ledger::open(LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    })
    .map_err(|err| anyhow::anyhow!("burn search: unable to open ledger/content store: {err}"))
    .inspect_err(|_| progress.finish_and_clear())?;

    progress.set_task("searching content");
    let result = handle
        .search(SearchQueryOptions {
            query: args.query,
            limit: requested_limit,
            session_id: args.session.clone(),
            ledger_home: None,
        })
        .map_err(|err| {
            anyhow::anyhow!(
                "burn search: content search failed (check FTS5 query syntax and content store): {err}"
            )
        })
        .inspect_err(|_| progress.finish_and_clear())?;
    progress.finish_and_clear();

    if globals.json {
        emit_json(&result, applied_limit, args.session.as_deref())?;
    } else {
        emit_human(
            &result,
            applied_limit,
            args.session.as_deref(),
            args.snippet,
            ux::colors_enabled(globals),
        );
    }
    Ok(0)
}

fn emit_json(result: &SearchResult, limit: usize, session: Option<&str>) -> std::io::Result<()> {
    let mut payload = json!({
        "query": result.query,
        "limit": limit,
        "truncated": result.hits.len() == limit,
        "session": session,
        "hits": result.hits,
    });
    coerce_whole_f64_to_int(&mut payload);
    render_json(&payload)
}

fn emit_human(
    result: &SearchResult,
    limit: usize,
    session: Option<&str>,
    include_snippets: bool,
    color: bool,
) {
    if result.hits.is_empty() {
        match session {
            Some(id) => println!("no content matches {:?} in session {id}.", result.query),
            None => println!("no content matches {:?}.", result.query),
        }
        return;
    }

    println!("\nsearch results for {:?}\n", result.query);
    println!("{}", render_table(&hit_table_rows(&result.hits)));

    if include_snippets {
        println!();
        for (index, hit) in result.hits.iter().enumerate() {
            println!("{}. {}", index + 1, render_snippet(&hit.snippet, color));
        }
    }

    println!(
        "\nshowing {} result{} (limit {limit})\n",
        result.hits.len(),
        if result.hits.len() == 1 { "" } else { "s" },
    );
}

fn hit_table_rows(hits: &[SearchHit]) -> Vec<Vec<String>> {
    let mut rows = vec![vec![
        "rank".into(),
        "source".into(),
        "session".into(),
        "message".into(),
    ]];
    rows.extend(hits.iter().enumerate().map(|(index, hit)| {
        vec![
            (index + 1).to_string(),
            hit.source.clone(),
            hit.session_id.clone(),
            hit.message_id.clone(),
        ]
    }));
    rows
}

fn render_snippet(snippet: &str, color: bool) -> String {
    if color {
        snippet
            .replace("<b>", "\u{1b}[1m")
            .replace("</b>", "\u{1b}[0m")
    } else {
        snippet.replace("<b>", "").replace("</b>", "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_snippets_strip_fts_markup() {
        assert_eq!(
            render_snippet("before <b>needle</b> after", false),
            "before needle after"
        );
    }

    #[test]
    fn color_snippets_translate_fts_markup_to_ansi_bold() {
        assert_eq!(
            render_snippet("<b>needle</b>", true),
            "\u{1b}[1mneedle\u{1b}[0m"
        );
    }
}
