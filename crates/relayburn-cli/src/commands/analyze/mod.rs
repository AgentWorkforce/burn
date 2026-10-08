//! `burn analyze` — every burn analyzer over one session, read through
//! relayhistory. No ledger and no ingest: `--json` emits the SDK's
//! `burn.session-analysis.v1` document unchanged.

use std::path::{Path, PathBuf};

use clap::Args;
use relayburn_sdk::{
    analyze_session, AnalyzeSessionOptions, Harness, HistoryStoreOptions, SessionLocator,
};

use crate::cli::{GlobalArgs, HarnessArg};
use crate::render::error::report_error;
use crate::render::json::render_json;
use crate::render::stdout::write_stdout;

mod human;

/// Per-command flags for `burn analyze`.
#[derive(Debug, Clone, Args)]
#[command(after_help = "Examples:
  burn analyze claude 2f6c1e0a-…            look a session up in relayhistory
  burn analyze --path ./session.jsonl       analyze a transcript file
  burn analyze --path rollout-….jsonl --json")]
pub struct AnalyzeArgs {
    /// Harness that recorded the session.
    #[arg(
        value_enum,
        value_name = "SOURCE",
        required_unless_present = "path",
        requires = "session_id"
    )]
    pub source: Option<HarnessArg>,

    /// Session id within that harness.
    #[arg(value_name = "SESSION_ID")]
    pub session_id: Option<String>,

    /// Analyze this session artifact instead: a Claude Code or Codex
    /// transcript, or an OpenCode `storage/session/<scope>/<id>.json`.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["source", "session_id"])]
    pub path: Option<PathBuf>,

    /// Harness format of `--path`. Inferred when omitted: `rollout-*`
    /// files are Codex, `.json` files OpenCode, anything else Claude Code.
    #[arg(long, value_enum, value_name = "HARNESS", requires = "path")]
    pub harness: Option<HarnessArg>,

    /// relayhistory (ai-hist) database that resolves session ids.
    #[arg(long, value_name = "PATH")]
    pub store_db: Option<PathBuf>,

    /// Provider home whose harness stores are searched, instead of `$HOME`.
    #[arg(long, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// Optional models.dev-compatible pricing overlay.
    #[arg(long, value_name = "PATH")]
    pub pricing: Option<PathBuf>,

    /// Project whose instruction files to price. Defaults to the session's
    /// working directory.
    #[arg(long, value_name = "DIR")]
    pub project: Option<PathBuf>,
}

pub fn run(globals: &GlobalArgs, args: AnalyzeArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(()) => 0,
        Err(error) => report_error(&error, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: AnalyzeArgs) -> anyhow::Result<()> {
    let analysis = analyze_session(options(args)?)?;
    if globals.json {
        render_json(&analysis)?;
        return Ok(());
    }
    write_stdout(&human::render(&analysis))?;
    Ok(())
}

fn options(args: AnalyzeArgs) -> anyhow::Result<AnalyzeSessionOptions> {
    let session = match (args.path, args.source, args.session_id) {
        (Some(path), _, _) => SessionLocator::Path {
            harness: args
                .harness
                .map(Harness::from)
                .unwrap_or_else(|| infer_harness(&path)),
            path,
        },
        (None, Some(source), Some(session_id)) => SessionLocator::Id {
            harness: source.into(),
            session_id,
        },
        _ => anyhow::bail!("pass <SOURCE> <SESSION_ID> or --path <PATH>"),
    };
    let mut options = AnalyzeSessionOptions::new(session);
    options.store = HistoryStoreOptions {
        db_path: args.store_db,
        home: args.home,
    };
    options.pricing_path = args.pricing;
    options.project_dir = args.project;
    Ok(options)
}

fn infer_harness(path: &Path) -> Harness {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.starts_with("rollout-") {
        Harness::Codex
    } else if name.ends_with(".json") {
        Harness::Opencode
    } else {
        Harness::ClaudeCode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_harness_is_inferred_from_the_file_name() {
        assert_eq!(
            infer_harness(Path::new("/x/rollout-2026-01-01T00-00-00-abc.jsonl")),
            Harness::Codex
        );
        assert_eq!(
            infer_harness(Path::new("storage/session/global/ses_1.json")),
            Harness::Opencode
        );
        assert_eq!(infer_harness(Path::new("abc.jsonl")), Harness::ClaudeCode);
    }
}
