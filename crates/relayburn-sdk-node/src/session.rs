//! Single-session verbs: `measureSession` and `analyzeSession`. Both read
//! one session through relayhistory; neither opens a ledger.

use std::path::PathBuf;

use napi::Error as NapiError;
use napi_derive::napi;

use crate::{invalid_arg, maybe_path, sdk, sdk_err, BigIntPromoting, BurnError, SDK_ERROR_CODE};

fn parse_harness(verb: &str, value: &str) -> Result<sdk::Harness, BurnError> {
    match value {
        "claude-code" | "claude" => Ok(sdk::Harness::ClaudeCode),
        "codex" => Ok(sdk::Harness::Codex),
        "opencode" => Ok(sdk::Harness::Opencode),
        other => Err(invalid_arg(format!(
            "{verb}: invalid harness {other:?} (expected claude-code, codex, or opencode)"
        ))),
    }
}

fn promoted(verb: &str, value: &impl serde::Serialize) -> Result<BigIntPromoting, BurnError> {
    serde_json::to_value(value)
        .map(BigIntPromoting)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize {verb}: {e}")))
}

#[napi(object)]
pub struct MeasureSessionOptions {
    pub input_path: String,
    pub harness: String,
    pub pricing_path: Option<String>,
}

/// Parse one exact session artifact and return Cloud-ready token/cost metrics.
#[napi(js_name = "measureSession")]
pub fn measure_session(opts: MeasureSessionOptions) -> Result<BigIntPromoting, BurnError> {
    let result = sdk::measure_session(sdk::MeasureSessionOptions {
        input_path: PathBuf::from(opts.input_path),
        harness: parse_harness("measureSession", &opts.harness)?,
        pricing_path: maybe_path(opts.pricing_path),
    })
    .map_err(sdk_err)?;
    promoted("measureSession", &result)
}

/// Which session `analyzeSession` reads: `sessionId` (looked up in a
/// relayhistory store) or `path` (a transcript or OpenCode session file).
#[napi(object)]
pub struct AnalyzeSessionOptions {
    pub harness: String,
    pub session_id: Option<String>,
    pub path: Option<String>,
    /// ai-hist database that resolves `sessionId`.
    pub store_db_path: Option<String>,
    /// Provider home searched for `sessionId`, instead of `$HOME`.
    pub home: Option<String>,
    pub pricing_path: Option<String>,
    /// Project whose instruction files to price; defaults to the session cwd.
    pub project_dir: Option<String>,
}

/// Every burn analyzer over one session; returns `burn.session-analysis.v1`.
#[napi(js_name = "analyzeSession")]
pub fn analyze_session(opts: AnalyzeSessionOptions) -> Result<BigIntPromoting, BurnError> {
    let harness = parse_harness("analyzeSession", &opts.harness)?;
    let session = match (opts.session_id, opts.path) {
        (Some(session_id), None) => sdk::SessionLocator::Id {
            harness,
            session_id,
        },
        (None, Some(path)) => sdk::SessionLocator::Path {
            harness,
            path: PathBuf::from(path),
        },
        _ => {
            return Err(invalid_arg(
                "analyzeSession: pass exactly one of sessionId or path",
            ))
        }
    };
    let mut options = sdk::AnalyzeSessionOptions::new(session);
    options.store = sdk::HistoryStoreOptions {
        db_path: maybe_path(opts.store_db_path),
        home: maybe_path(opts.home),
    };
    options.pricing_path = maybe_path(opts.pricing_path);
    options.project_dir = maybe_path(opts.project_dir);
    let analysis = sdk::analyze_session(options).map_err(sdk_err)?;
    promoted("analyzeSession", &analysis)
}
