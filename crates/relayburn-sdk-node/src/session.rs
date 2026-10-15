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
    let session = locator(harness, opts.session_id, opts.path)?;
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

/// The session `analyzeSession` names: exactly one of an id or a path.
fn locator(
    harness: sdk::Harness,
    session_id: Option<String>,
    path: Option<String>,
) -> Result<sdk::SessionLocator, BurnError> {
    match (session_id, path) {
        (Some(session_id), None) => Ok(sdk::SessionLocator::Id {
            harness,
            session_id,
        }),
        (None, Some(path)) => Ok(sdk::SessionLocator::Path {
            harness,
            path: PathBuf::from(path),
        }),
        _ => Err(invalid_arg(
            "analyzeSession: pass exactly one of sessionId or path",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use serde_json::Value;

    use super::locator;
    use crate::{is_bigint_field, sdk};

    #[test]
    fn a_session_is_named_by_exactly_one_of_id_or_path() {
        let harness = sdk::Harness::Codex;
        assert!(matches!(
            locator(harness, Some("s1".into()), None),
            Ok(sdk::SessionLocator::Id { session_id, .. }) if session_id == "s1"
        ));
        assert!(matches!(
            locator(harness, None, Some("/x.jsonl".into())),
            Ok(sdk::SessionLocator::Path { path, .. }) if path == Path::new("/x.jsonl")
        ));
        assert!(locator(harness, None, None).is_err());
        assert!(locator(harness, Some("s1".into()), Some("/x.jsonl".into())).is_err());
    }

    /// Integer fields the facade does not type as `number | bigint`: `depth`
    /// is an `i32`, `turnIndexes` is declared `number[]`, and the rest live
    /// in sections `index.d.ts` types as `Record<string, unknown>` (fidelity
    /// summaries, quality outcomes).
    const SMALL_INTEGER_FIELDS: &[&str] = &[
        "depth",
        "turnIndexes",
        "full",
        "usage-only",
        "aggregate-only",
        "cost-only",
        "none",
        "per-turn",
        "per-message",
        "per-session-aggregate",
        "hasInputTokens",
        "hasOutputTokens",
        "hasReasoningTokens",
        "hasCacheReadTokens",
        "hasCacheCreateTokens",
        "hasToolCalls",
        "hasToolResultEvents",
        "hasSessionRelationships",
        "hasRawContent",
        "maxOutputBytes",
        "totalOutputBytes",
        "totalRetries",
        "truncatedCount",
    ];

    fn integer_keys(value: &Value, key: Option<&str>, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    integer_keys(v, Some(k), out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    integer_keys(item, key, out);
                }
            }
            Value::Number(n) if n.is_u64() || n.is_i64() => {
                if let Some(key) = key {
                    out.insert(key.to_string());
                }
            }
            _ => {}
        }
    }

    /// Every integer field `analyzeSession` returns for a session with
    /// nested subagents crosses to JS as `BigInt`.
    #[test]
    fn analyze_session_promotes_every_integer_field() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/claude-sidecars/sidecar-session.jsonl");
        let analysis =
            sdk::analyze_session(sdk::AnalyzeSessionOptions::new(sdk::SessionLocator::Path {
                harness: sdk::Harness::ClaudeCode,
                path: fixture,
            }))
            .unwrap();
        let mut keys = BTreeSet::new();
        integer_keys(&serde_json::to_value(&analysis).unwrap(), None, &mut keys);
        assert!(keys.contains("cumulativeTokens"), "{keys:?}");
        let missing: Vec<&String> = keys
            .iter()
            .filter(|k| !is_bigint_field(k) && !SMALL_INTEGER_FIELDS.contains(&k.as_str()))
            .collect();
        assert!(missing.is_empty(), "not promoted to BigInt: {missing:?}");
    }
}
