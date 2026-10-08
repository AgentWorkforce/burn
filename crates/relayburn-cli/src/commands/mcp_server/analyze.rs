//! `burn__analyzeSession` — the ledger-less single-session analysis as an
//! MCP tool. Defaults to the server's registered session.

use std::path::PathBuf;

use relayburn_sdk::{analyze_session, AnalyzeSessionOptions, Harness, SessionLocator};
use serde_json::{json, Value};

use super::{object_input, optional_string, tool_error, tool_output};

const TOOL: &str = "burn__analyzeSession";
const FIELDS: &[&str] = &[
    "harness",
    "sessionId",
    "path",
    "storeDbPath",
    "home",
    "pricingPath",
    "projectDir",
];

pub(super) fn catalog_entry() -> Value {
    json!({
        "name": TOOL,
        "description": "Diagnose one session's token spend without the burn ledger: metrics per model, activity, cost hotspots, instruction-file overhead, subagents, context growth, quality, and a ranked findings list where each finding explains what happened, why it costs tokens, its evidence and impact, and a concrete fix. Pass sessionId (defaults to the server's registered session) or path. Returns the burn.session-analysis.v1 document. Read-only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "harness": { "type": "string", "enum": ["claude-code", "claude", "codex", "opencode"], "description": "Harness that recorded the session." },
                "sessionId": { "type": "string", "description": "Session id, resolved through relayhistory. Omit to use the server registered session." },
                "path": { "type": "string", "description": "Session artifact to analyze instead: a Claude Code or Codex transcript, or an OpenCode storage/session/<scope>/<id>.json." },
                "storeDbPath": { "type": "string", "description": "ai-hist database that resolves sessionId." },
                "home": { "type": "string", "description": "Provider home searched for sessionId, instead of $HOME." },
                "pricingPath": { "type": "string", "description": "models.dev-compatible pricing overlay." },
                "projectDir": { "type": "string", "description": "Project whose instruction files to price; defaults to the session cwd." }
            },
            "required": ["harness"],
            "additionalProperties": false
        }
    })
}

pub(super) fn call(args: &Value, default_session: Option<&str>) -> Value {
    match options(args, default_session) {
        Ok(options) => match analyze_session(options) {
            Ok(analysis) => tool_output(&analysis),
            Err(err) => tool_error(format!("{err:#}")),
        },
        Err(err) => tool_error(err),
    }
}

fn options(args: &Value, default_session: Option<&str>) -> Result<AnalyzeSessionOptions, String> {
    let input = object_input(args, TOOL, FIELDS)?;
    let field = |key: &str| optional_string(input, key, TOOL);
    let harness = match field("harness")?.as_deref() {
        Some("claude-code" | "claude") => Harness::ClaudeCode,
        Some("codex") => Harness::Codex,
        Some("opencode") => Harness::Opencode,
        Some(other) => return Err(format!("{TOOL}: unknown harness {other}")),
        None => return Err(format!("{TOOL}: harness is required")),
    };
    let session = match (field("path")?, field("sessionId")?) {
        (Some(_), Some(_)) => return Err(format!("{TOOL}: pass at most one of path / sessionId")),
        (Some(path), None) => SessionLocator::Path {
            harness,
            path: PathBuf::from(path),
        },
        (None, session_id) => SessionLocator::Id {
            harness,
            session_id: session_id
                .or_else(|| default_session.map(str::to_string))
                .ok_or_else(|| {
                    format!("{TOOL}: pass sessionId or path; the server has no registered session")
                })?,
        },
    };
    let mut options = AnalyzeSessionOptions::new(session);
    options.store.db_path = field("storeDbPath")?.map(PathBuf::from);
    options.store.home = field("home")?.map(PathBuf::from);
    options.pricing_path = field("pricingPath")?.map(PathBuf::from);
    options.project_dir = field("projectDir")?.map(PathBuf::from);
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(path)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn analyzes_a_transcript_path() {
        let result = call(
            &json!({ "harness": "claude", "path": fixture("claude/retry-loop.jsonl") }),
            None,
        );
        let analysis = &result["structuredContent"];
        assert_eq!(analysis["schema"], "burn.session-analysis.v1");
        assert_eq!(analysis["findings"][0]["code"], "retry-loop");
        assert!(analysis.get("ledgerFreshness").is_none());
    }

    #[test]
    fn invalid_inputs_are_tool_errors() {
        for (args, message) in [
            (json!({}), "harness is required"),
            (json!({ "harness": "cursor" }), "unknown harness cursor"),
            (json!({ "harness": "codex" }), "no registered session"),
            (
                json!({ "harness": "codex", "path": "/x", "sessionId": "s" }),
                "at most one of path / sessionId",
            ),
            (
                json!({ "harness": "codex", "extra": 1 }),
                "unknown property extra",
            ),
        ] {
            let result = call(&args, None);
            assert_eq!(result["isError"], true, "{args}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(message), "{text}");
        }
    }

    #[test]
    fn registered_session_is_the_default_id() {
        let home = tempfile::tempdir().unwrap();
        let args = json!({
            "harness": "codex",
            "home": home.path(),
            "storeDbPath": home.path().join("ai-history.db"),
        });
        let result = call(&args, Some("registered-session"));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("codex session registered-session"), "{text}");
    }
}
