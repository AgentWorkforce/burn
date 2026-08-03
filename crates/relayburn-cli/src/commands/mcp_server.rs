//! `burn mcp-server` — stdio MCP server exposing read-only ledger
//! queries for in-session self-query (closes #210).
//!
//! The TS sibling (`packages/mcp/src/server.ts`) hand-rolls a minimal
//! JSON-RPC 2.0 line-delimited server rather than depending on a heavy
//! SDK; the Rust port mirrors that decision. The on-wire shape is tiny
//! (`initialize`, `ping`, `tools/list`, `tools/call`, plus
//! notifications), and freezing a specific `rmcp` version buys us
//! nothing for the read-only surface this command exposes. If the
//! protocol evolves, this module is localized enough to update in one
//! place — same trade-off the TS sibling makes.
//!
//! The seven read-only tools mirror `packages/mcp/src/tools/*.ts`: session
//! cost, fingerprint, summary, hotspots, overhead attribution, overhead
//! trimming, and model comparison. Every tool is a thin wrapper around a
//! [`LedgerHandle`] verb; this presenter owns only MCP schema validation and
//! result framing.

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use relayburn_sdk::{
    CompareOptions, Enrichment, FingerprintScope, HotspotsOptions, Ledger, LedgerHandle,
    LedgerOpenOptions, OverheadOptions, OverheadTrimOptions, SessionCostOptions, SessionCostResult,
    SummaryOptions,
};

use crate::cli::{GlobalArgs, McpServerArgs};
use crate::render::error::report_error;

/// Latest MCP protocol revision we know how to speak. Clients negotiate;
/// we echo the client's declared version when present (treating it as a
/// superset declaration), else fall back to this baseline. Mirrors
/// `packages/mcp/src/server.ts`.
const PROTOCOL_VERSION: &str = "2025-03-26";
/// Server name surfaced in the `initialize` reply. Kept distinct from
/// the binary name so MCP clients can disambiguate the Rust port from
/// the TS one in their server inventories.
const SERVER_NAME: &str = "relayburn-mcp";
/// Server version surfaced in the `initialize` reply. Bumped manually
/// when the tool surface changes; `cargo` doesn't let us read the
/// package version at runtime without `env!`.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run(globals: &GlobalArgs, args: McpServerArgs) -> i32 {
    // Open the ledger up front so a config error fails loud before any
    // MCP traffic flows. The handle is held by the tool dispatcher for
    // the life of the server — one connection per process matches the
    // TS server.
    let handle = match open_handle(globals) {
        Ok(h) => h,
        Err(err) => return report_error(&err, globals),
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => return report_error(&err, globals),
    };

    let server = Server {
        handle: Arc::new(tokio::sync::Mutex::new(handle)),
        default_session_id: args.session_id.clone(),
        debug: args.debug,
    };

    rt.block_on(server.run());
    0
}

fn open_handle(globals: &GlobalArgs) -> anyhow::Result<LedgerHandle> {
    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    Ledger::open(opts)
}

// ---------------------------------------------------------------------------
// JSON-RPC envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    /// Request id is optional in JSON-RPC: when absent the message is a
    /// notification and we must not reply. We deserialize it as a `Value`
    /// to preserve numeric / string id types verbatim on the way back —
    /// MCP clients use both shapes.
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcSuccess<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    error: JsonRpcErrorBody,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorBody {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

struct Server {
    handle: Arc<tokio::sync::Mutex<LedgerHandle>>,
    default_session_id: Option<String>,
    debug: bool,
}

impl Server {
    async fn run(self) {
        // Read line-delimited JSON-RPC frames off stdin. Tokio doesn't
        // give us a stable cross-platform stdin AsyncBufRead without
        // pulling more deps, and the MCP spec is one frame per line, so
        // a blocking BufRead loop on a dedicated thread is the cleanest
        // shape. We marshal each frame back into the runtime via a
        // bounded channel so tool handlers can use the SDK's async
        // surface.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let stdin_thread = std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let lock = stdin.lock();
            for line in lock.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });

        while let Some(frame) = rx.recv().await {
            self.handle_frame(&frame).await;
        }

        let _ = stdin_thread.join();
    }

    async fn handle_frame(&self, frame: &str) {
        let parsed: serde_json::Result<Value> = serde_json::from_str(frame);
        let value = match parsed {
            Ok(v) => v,
            Err(err) => {
                if self.debug {
                    eprintln!("[burn mcp] parse error: {err}");
                }
                write_response(&error_envelope(&Value::Null, -32700, "parse error", None));
                return;
            }
        };
        if !value.is_object() {
            write_response(&error_envelope(
                &Value::Null,
                -32600,
                "invalid request",
                None,
            ));
            return;
        }

        // Notifications carry no `id` field. Per JSON-RPC 2.0 we must
        // not reply to them. The MCP spec uses `notifications/initialized`
        // and `notifications/cancelled`; both are safe to ignore for a
        // tools-only server.
        let has_id = value.get("id").is_some();
        if !has_id {
            return;
        }

        let req: JsonRpcRequest = match serde_json::from_value(value.clone()) {
            Ok(r) => r,
            Err(err) => {
                if self.debug {
                    eprintln!("[burn mcp] bad request shape: {err}");
                }
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                write_response(&error_envelope(&id, -32600, "invalid request", None));
                return;
            }
        };
        // Unwrap is safe: we already confirmed the field is present
        // above. Default to `null` defensively so a misbehaving client
        // can't crash the server by sending `id: null`.
        let id = req.id.unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => self.handle_initialize(&id, &req.params),
            "ping" => write_success(&id, json!({})),
            "tools/list" => self.handle_tools_list(&id),
            "tools/call" => self.handle_tools_call(&id, &req.params).await,
            other => {
                write_response(&error_envelope(
                    &id,
                    -32601,
                    &format!("method not found: {other}"),
                    None,
                ));
            }
        }
    }

    fn handle_initialize(&self, id: &Value, params: &Value) {
        let client_version = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let protocol_version = client_version.unwrap_or_else(|| PROTOCOL_VERSION.to_string());
        let result = json!({
            "protocolVersion": protocol_version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        });
        write_success(id, result);
    }

    fn handle_tools_list(&self, id: &Value) {
        write_success(id, json!({ "tools": tool_catalog() }));
    }

    async fn handle_tools_call(&self, id: &Value, params: &Value) {
        let name = params.get("name").and_then(|v| v.as_str());
        let Some(name) = name else {
            write_response(&error_envelope(
                id,
                -32602,
                "tools/call requires a name",
                None,
            ));
            return;
        };
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match self.call_tool(name, &args).await {
            Some(result) => write_success(id, result),
            None => write_response(&error_envelope(
                id,
                -32601,
                &format!("unknown tool: {name}"),
                None,
            )),
        }
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Option<Value> {
        Some(match name {
            "burn__sessionCost" => self.tool_session_cost(args).await,
            "burn__fingerprint" => self.tool_fingerprint(args).await,
            "burn__summary" => self.tool_summary(args).await,
            "burn__hotspots" => self.tool_hotspots(args).await,
            "burn__overhead" => self.tool_overhead(args).await,
            "burn__overheadTrim" => self.tool_overhead_trim(args).await,
            "burn__compare" => self.tool_compare(args).await,
            _ => return None,
        })
    }

    async fn tool_fingerprint(&self, args: &Value) -> Value {
        // Empty / missing args → AllSessions. `sessionId` and `project`
        // are mutually exclusive; if both are present, fail loud at
        // tool-error level rather than silently picking one.
        let session = args
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let project = args
            .get("project")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let scope = match (session, project) {
            (Some(_), Some(_)) => {
                return tool_error("fingerprint: pass at most one of sessionId / project");
            }
            (Some(s), None) => FingerprintScope::Session(s),
            (None, Some(p)) => FingerprintScope::Project(p),
            (None, None) => FingerprintScope::AllSessions,
        };

        let handle_guard = self.handle.lock().await;
        let result = handle_guard.fingerprint(scope);
        drop(handle_guard);

        match result {
            Ok(fp) => tool_output(&json!({ "fingerprint": fp.as_str() })),
            Err(err) => tool_error(err),
        }
    }

    async fn tool_session_cost(&self, args: &Value) -> Value {
        let override_id = args
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let session = override_id
            .clone()
            .or_else(|| self.default_session_id.clone());

        let opts = SessionCostOptions {
            session: session.clone(),
            ledger_home: None,
        };
        let handle_guard = self.handle.lock().await;
        let result = handle_guard.session_cost(opts);
        drop(handle_guard);

        let mut payload: SessionCostResult = match result {
            Ok(r) => r,
            Err(err) => return tool_error(err),
        };

        // Mirror TS: when no override and no registered default, surface
        // a more descriptive note than the SDK's generic one.
        if payload.session_id.is_none()
            && override_id.is_none()
            && self.default_session_id.is_none()
        {
            payload.note =
                Some("no session id provided and server was not registered with one".to_string());
        }
        tool_output(&payload)
    }

    async fn tool_summary(&self, args: &Value) -> Value {
        let input = match object_input(
            args,
            "summary",
            &["session", "project", "since", "tags", "groupByTag"],
        ) {
            Ok(input) => input,
            Err(err) => return tool_error(err),
        };
        let opts = match (|| -> Result<SummaryOptions, String> {
            Ok(SummaryOptions {
                session: optional_string(input, "session", "summary")?
                    .or_else(|| self.default_session_id.clone()),
                project: optional_string(input, "project", "summary")?,
                since: optional_string(input, "since", "summary")?,
                tags: optional_string_record(input, "tags", "summary")?,
                group_by_tag: optional_string(input, "groupByTag", "summary")?,
                // Ledger selection belongs to the server's pre-opened handle.
                ledger_home: None,
            })
        })() {
            Ok(opts) => opts,
            Err(err) => return tool_error(err),
        };
        let handle = self.handle.lock().await;
        match handle.summary(opts) {
            Ok(result) => tool_output(&result),
            Err(err) => tool_error(err),
        }
    }

    async fn tool_hotspots(&self, args: &Value) -> Value {
        let input = match object_input(
            args,
            "hotspots",
            &[
                "session", "project", "since", "groupBy", "patterns", "workflow", "provider",
            ],
        ) {
            Ok(input) => input,
            Err(err) => return tool_error(err),
        };
        let opts = match (|| -> Result<HotspotsOptions, String> {
            Ok(HotspotsOptions {
                session: optional_string(input, "session", "hotspots")?
                    .or_else(|| self.default_session_id.clone()),
                project: optional_string(input, "project", "hotspots")?,
                since: optional_string(input, "since", "hotspots")?,
                group_by: optional_enum(input, "groupBy", "hotspots")?,
                patterns: optional_string_array(input, "patterns", "hotspots")?,
                workflow: optional_string(input, "workflow", "hotspots")?,
                provider: optional_string_array(input, "provider", "hotspots")?,
                ledger_home: None,
            })
        })() {
            Ok(opts) => opts,
            Err(err) => return tool_error(err),
        };
        let handle = self.handle.lock().await;
        match handle.hotspots(opts) {
            Ok(result) => tool_output(&result),
            Err(err) => tool_error(err),
        }
    }

    async fn tool_overhead(&self, args: &Value) -> Value {
        let input = match object_input(args, "overhead", &["project", "since", "kind"]) {
            Ok(input) => input,
            Err(err) => return tool_error(err),
        };
        let opts = match (|| -> Result<OverheadOptions, String> {
            Ok(OverheadOptions {
                project: optional_string(input, "project", "overhead")?.map(Into::into),
                since: optional_string(input, "since", "overhead")?,
                kind: optional_enum(input, "kind", "overhead")?,
                ledger_home: None,
            })
        })() {
            Ok(opts) => opts,
            Err(err) => return tool_error(err),
        };
        let handle = self.handle.lock().await;
        match handle.overhead(opts) {
            Ok(result) => tool_output(&result),
            Err(err) => tool_error(err),
        }
    }

    async fn tool_overhead_trim(&self, args: &Value) -> Value {
        let input = match object_input(
            args,
            "overhead trim",
            &["project", "since", "kind", "top", "includeDiff"],
        ) {
            Ok(input) => input,
            Err(err) => return tool_error(err),
        };
        let opts = match (|| -> Result<OverheadTrimOptions, String> {
            let top = optional_u32(input, "top", "overhead trim")?;
            if top == Some(0) {
                return Err("overhead trim: top must be a positive safe integer".to_string());
            }
            Ok(OverheadTrimOptions {
                project: optional_string(input, "project", "overhead trim")?.map(Into::into),
                since: optional_string(input, "since", "overhead trim")?,
                kind: optional_enum(input, "kind", "overhead trim")?,
                ledger_home: None,
                top: top.map(u64::from),
                include_diff: optional_boolean(input, "includeDiff", "overhead trim")?,
            })
        })() {
            Ok(opts) => opts,
            Err(err) => return tool_error(err),
        };
        let handle = self.handle.lock().await;
        match handle.overhead_trim(opts) {
            Ok(result) => tool_output(&result),
            Err(err) => tool_error(err),
        }
    }

    async fn tool_compare(&self, args: &Value) -> Value {
        let input = match object_input(
            args,
            "compare",
            &[
                "models",
                "session",
                "project",
                "since",
                "workflow",
                "agent",
                "provider",
                "minSample",
                "minFidelity",
            ],
        ) {
            Ok(input) => input,
            Err(err) => return tool_error(err),
        };
        let opts = match (|| -> Result<CompareOptions, String> {
            let models = required_string_array(input, "models", "compare", 2)?;
            Ok(CompareOptions {
                models,
                session: optional_string(input, "session", "compare")?,
                project: optional_string(input, "project", "compare")?,
                since: optional_string(input, "since", "compare")?,
                workflow: optional_string(input, "workflow", "compare")?,
                agent: optional_string(input, "agent", "compare")?,
                provider: optional_string_array(input, "provider", "compare")?,
                min_sample: optional_u32(input, "minSample", "compare")?.map(u64::from),
                min_fidelity: optional_enum(input, "minFidelity", "compare")?,
                ledger_home: None,
            })
        })() {
            Ok(opts) => opts,
            Err(err) => return tool_error(err),
        };
        let handle = self.handle.lock().await;
        match handle.compare(opts) {
            Ok(result) => tool_output(&result),
            Err(err) => tool_error(err),
        }
    }
}

fn tool_catalog() -> Value {
    json!([
        {
            "name": "burn__sessionCost",
            "description":
                "Return the total cost (USD), token count, and turn count for a session. \
                 Defaults to the server's registered sessionId (the running agent's own \
                 session). Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": {
                        "type": "string",
                        "description":
                            "Override the registered session id. Omit to query the running \
                             agent's own session.",
                    },
                },
                "required": [],
                "additionalProperties": false,
            },
        },
        {
            "name": "burn__fingerprint",
            "description":
                "Cheap polling primitive over the burn ledger. Returns \
                 `{count}:{maxMtimeUnix}:{totalBytes}` — three integers \
                 joined by colons. Clients keep the last-seen value and \
                 skip re-querying when it's unchanged. Optionally scoped \
                 to a session id or a project path. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": {
                        "type": "string",
                        "description":
                            "Restrict to a single session_id. Mutually exclusive with project.",
                    },
                    "project": {
                        "type": "string",
                        "description":
                            "Restrict to rows whose project path matches. Mutually exclusive \
                             with sessionId.",
                    },
                },
                "required": [],
                "additionalProperties": false,
            },
        },
        {
            "name": "burn__summary",
            "description": "Summarize token use and cost by tool and model, optionally filtered by session, project, time window, or enrichment tags. When the server has a registered default session, omitting session restricts the query to it. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Restrict to one session id. Omit to use the server registered session when present." },
                    "project": { "type": "string", "description": "Restrict to one project path or key." },
                    "since": { "type": "string", "description": "ISO timestamp or relative range such as 24h or 7d." },
                    "tags": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Folded enrichment tags; every key/value pair must match." },
                    "groupByTag": { "type": "string", "description": "Group totals by this folded enrichment tag key." }
                },
                "required": [], "additionalProperties": false
            }
        },
        {
            "name": "burn__hotspots",
            "description": "Find expensive tool-output persistence and repeated workflow patterns, with attribution or grouped findings views. When the server has a registered default session, omitting session restricts the query to it. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Restrict to one session id. Omit to use the server registered session when present." },
                    "project": { "type": "string", "description": "Restrict to one project path or key." },
                    "since": { "type": "string", "description": "ISO timestamp or relative range such as 24h or 7d." },
                    "groupBy": { "type": "string", "enum": ["attribution", "bash", "bash-verb", "file", "subagent", "findings"], "description": "Select the hotspot result view." },
                    "patterns": { "type": "array", "items": { "type": "string" }, "description": "Only include matching finding patterns. A non-empty list selects findings mode." },
                    "workflow": { "type": "string", "description": "Restrict to a folded workflowId enrichment stamp." },
                    "provider": { "type": "array", "items": { "type": "string" }, "description": "Case-insensitive provider allow-list." }
                },
                "required": [], "additionalProperties": false
            }
        },
        {
            "name": "burn__overhead",
            "description": "Attribute CLAUDE.md and AGENTS.md instruction-file token overhead and cost by file and section. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project path or key. The SDK defaults to the current project." },
                    "since": { "type": "string", "description": "ISO timestamp or relative range such as 24h or 7d." },
                    "kind": { "type": "string", "enum": ["claude-md", "agents-md"], "description": "Restrict to one instruction-file kind." }
                },
                "required": [], "additionalProperties": false
            }
        },
        {
            "name": "burn__overheadTrim",
            "description": "Recommend high-cost instruction-file sections to trim and estimate their savings, optionally with suggested diffs. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project path or key. The SDK defaults to the current project." },
                    "since": { "type": "string", "description": "ISO timestamp or relative range such as 24h or 7d." },
                    "kind": { "type": "string", "enum": ["claude-md", "agents-md"], "description": "Restrict to one instruction-file kind." },
                    "top": { "type": "integer", "minimum": 1, "maximum": 4294967295_u64, "description": "Maximum number of recommendations." },
                    "includeDiff": { "type": "boolean", "description": "Include a suggested edit diff for each recommendation." }
                },
                "required": [], "additionalProperties": false
            }
        },
        {
            "name": "burn__compare",
            "description": "Compare cost and outcome metrics across at least two models, grouped by activity category. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "models": { "type": "array", "items": { "type": "string" }, "minItems": 2, "description": "Model names to compare." },
                    "session": { "type": "string", "description": "Restrict to one session id." },
                    "project": { "type": "string", "description": "Restrict to one project path or key." },
                    "since": { "type": "string", "description": "ISO timestamp or relative range such as 24h or 7d." },
                    "workflow": { "type": "string", "description": "Restrict to a folded workflowId enrichment stamp." },
                    "agent": { "type": "string", "description": "Restrict to a folded agentId enrichment stamp." },
                    "provider": { "type": "array", "items": { "type": "string" }, "description": "Case-insensitive provider allow-list." },
                    "minSample": { "type": "integer", "minimum": 0, "maximum": 4294967295_u64, "description": "Minimum observations before a comparison cell is sufficient." },
                    "minFidelity": { "type": "string", "enum": ["full", "usage-only", "aggregate-only", "cost-only", "partial"], "description": "Minimum accepted telemetry fidelity." }
                },
                "required": ["models"], "additionalProperties": false
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Tool input validation + result framing
// ---------------------------------------------------------------------------

fn object_input<'a>(
    raw: &'a Value,
    tool: &str,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    let Some(input) = raw.as_object() else {
        return Err(format!("{tool}: input must be an object"));
    };
    if let Some(key) = input.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{tool}: unknown property {key}"));
    }
    Ok(input)
}

fn optional_string(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<String>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| format!("{tool}: {key} must be a string"))
}

fn optional_boolean(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{tool}: {key} must be a boolean"))
}

fn optional_u32(input: &Map<String, Value>, key: &str, tool: &str) -> Result<Option<u32>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(value) = value.as_u64().and_then(|value| u32::try_from(value).ok()) else {
        return Err(format!("{tool}: {key} must be a 32-bit unsigned integer"));
    };
    Ok(Some(value))
}

fn optional_string_array(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(format!("{tool}: {key} must be an array of strings"));
    };
    let values: Option<Vec<String>> = items
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect();
    values
        .map(Some)
        .ok_or_else(|| format!("{tool}: {key} must be an array of strings"))
}

fn required_string_array(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
    minimum: usize,
) -> Result<Vec<String>, String> {
    let value = optional_string_array(input, key, tool)?;
    match value {
        Some(items) if items.len() >= minimum => Ok(items),
        _ => Err(format!(
            "{tool}: {key} must contain at least {minimum} strings"
        )),
    }
}

fn optional_string_record(
    input: &Map<String, Value>,
    key: &str,
    tool: &str,
) -> Result<Option<Enrichment>, String> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    let Some(record) = value.as_object() else {
        return Err(format!(
            "{tool}: {key} must be an object with string values"
        ));
    };
    let mut result = Enrichment::new();
    for (record_key, value) in record {
        let Some(value) = value.as_str() else {
            return Err(format!(
                "{tool}: {key} must be an object with string values"
            ));
        };
        result.insert(record_key.clone(), value.to_string());
    }
    Ok(Some(result))
}

fn optional_enum<T>(input: &Map<String, Value>, key: &str, tool: &str) -> Result<Option<T>, String>
where
    T: DeserializeOwned,
{
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    if !value.is_string() {
        return Err(format!("{tool}: {key} must be a string"));
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|err| format!("{tool}: invalid {key}: {err}"))
}

fn tool_error(err: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": err.to_string() }],
        "isError": true,
    })
}

fn tool_output(payload: &impl Serialize) -> Value {
    match serde_json::to_value(payload) {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(text) => json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": value,
            }),
            Err(err) => tool_error(format!("failed to encode tool result: {err}")),
        },
        Err(err) => tool_error(format!("failed to encode tool result: {err}")),
    }
}

// ---------------------------------------------------------------------------
// Wire I/O
// ---------------------------------------------------------------------------

fn write_success(id: &Value, result: Value) {
    let env = JsonRpcSuccess {
        jsonrpc: "2.0",
        id,
        result,
    };
    write_response(&serde_json::to_value(&env).unwrap_or(Value::Null));
}

fn error_envelope(id: &Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let env = JsonRpcError {
        jsonrpc: "2.0",
        id,
        error: JsonRpcErrorBody {
            code,
            message: message.to_string(),
            data,
        },
    };
    serde_json::to_value(&env).unwrap_or(Value::Null)
}

fn write_response(value: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Ok(mut s) = serde_json::to_string(value) {
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_sdk::{SourceKind, ToolCall, TurnRecord, Usage};

    fn fixture_turn(index: u64, model: &str, project: &str) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "fixture-session".to_string(),
            session_path: None,
            message_id: format!("message-{index}"),
            turn_index: index,
            ts: format!("2026-08-03T12:00:0{index}.000Z"),
            model: model.to_string(),
            project: Some(project.to_string()),
            project_key: None,
            usage: Usage {
                input: 100,
                output: 20,
                ..Default::default()
            },
            tool_calls: vec![ToolCall {
                id: format!("tool-{index}"),
                name: "Read".to_string(),
                target: Some("AGENTS.md".to_string()),
                args_hash: "fixture".to_string(),
                is_error: None,
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            }],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn fixture_server() -> (Server, tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().expect("temp ledger home");
        let project = home.path().join("project");
        std::fs::create_dir(&project).expect("create fixture project");
        std::fs::write(
            project.join("AGENTS.md"),
            "# Fixture instructions\n\nKeep tests deterministic.\n",
        )
        .expect("write fixture instruction file");
        let mut handle =
            Ledger::open(LedgerOpenOptions::with_home(home.path())).expect("open fixture ledger");
        let project_string = project.to_string_lossy().into_owned();
        handle
            .raw_mut()
            .append_turns(&[
                fixture_turn(1, "claude-sonnet-4-6", &project_string),
                fixture_turn(2, "gpt-5.4", &project_string),
            ])
            .expect("append fixture turns");
        (
            Server {
                handle: Arc::new(tokio::sync::Mutex::new(handle)),
                default_session_id: Some("fixture-session".to_string()),
                debug: false,
            },
            home,
            project,
        )
    }

    fn assert_tool_success(result: &Value) -> &Value {
        assert_ne!(result.get("isError"), Some(&Value::Bool(true)), "{result}");
        let structured = result
            .get("structuredContent")
            .unwrap_or_else(|| panic!("missing structuredContent: {result}"));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("missing text content: {result}"));
        let text_value: Value = serde_json::from_str(text).expect("tool text is JSON");
        assert_eq!(
            &text_value, structured,
            "text and structured result diverged"
        );
        structured
    }

    /// The wire protocol is small enough to unit-test the framing
    /// helpers without spinning up a full server.
    #[test]
    fn error_envelope_carries_code_and_message() {
        let v = error_envelope(&json!(7), -32601, "method not found: foo", None);
        assert_eq!(v.get("jsonrpc"), Some(&Value::String("2.0".into())));
        assert_eq!(v.get("id"), Some(&json!(7)));
        let err = v.get("error").unwrap();
        assert_eq!(err.get("code"), Some(&json!(-32601)));
        assert_eq!(
            err.get("message"),
            Some(&Value::String("method not found: foo".into())),
        );
    }

    #[test]
    fn tools_list_catalog_contains_all_seven_tools_and_mirrors_numeric_caps() {
        let tools = tool_catalog();
        let names: Vec<&str> = tools
            .as_array()
            .expect("catalog array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        assert_eq!(
            names,
            [
                "burn__sessionCost",
                "burn__fingerprint",
                "burn__summary",
                "burn__hotspots",
                "burn__overhead",
                "burn__overheadTrim",
                "burn__compare",
            ]
        );
        assert_eq!(
            tools[5]["inputSchema"]["properties"]["top"]["maximum"],
            json!(u32::MAX)
        );
        assert_eq!(
            tools[6]["inputSchema"]["properties"]["minSample"]["maximum"],
            json!(u32::MAX)
        );
    }

    #[tokio::test]
    async fn new_tools_invoke_sdk_verbs_against_fixture_ledger() {
        let (server, _home, project) = fixture_server();
        let project = project.to_string_lossy();

        let summary = server
            .call_tool("burn__summary", &json!({}))
            .await
            .expect("known summary tool");
        assert_eq!(assert_tool_success(&summary)["turnCount"], json!(2));

        let hotspots = server
            .call_tool(
                "burn__hotspots",
                &json!({ "groupBy": "findings", "patterns": ["unpriced-usage"] }),
            )
            .await
            .expect("known hotspots tool");
        assert_eq!(assert_tool_success(&hotspots)["kind"], json!("findings"));

        let overhead = server
            .call_tool(
                "burn__overhead",
                &json!({ "project": project, "kind": "agents-md" }),
            )
            .await
            .expect("known overhead tool");
        assert_eq!(
            assert_tool_success(&overhead)["files"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );

        let trim = server
            .call_tool(
                "burn__overheadTrim",
                &json!({ "project": project, "kind": "agents-md", "top": 1, "includeDiff": false }),
            )
            .await
            .expect("known overhead trim tool");
        assert!(assert_tool_success(&trim)["summary"].is_object());

        let compare = server
            .call_tool(
                "burn__compare",
                &json!({
                    "models": ["claude-sonnet-4-6", "gpt-5.4"],
                    "session": "fixture-session",
                    "minFidelity": "partial"
                }),
            )
            .await
            .expect("known compare tool");
        assert_eq!(assert_tool_success(&compare)["analyzedTurns"], json!(2));
    }

    #[tokio::test]
    async fn invalid_inputs_are_tool_errors_without_structured_content_and_server_continues() {
        let (server, _home, _project) = fixture_server();
        let cases = [
            ("burn__summary", json!([])),
            ("burn__summary", json!("x")),
            ("burn__summary", Value::Null),
            ("burn__summary", json!({ "unknown": true })),
            ("burn__summary", json!({ "tags": { "bad": 1 } })),
            ("burn__hotspots", json!({ "groupBy": "unknown" })),
            ("burn__overhead", json!({ "kind": "readme" })),
            ("burn__overheadTrim", json!({ "top": 0 })),
            ("burn__overheadTrim", json!({ "top": 4294967296_u64 })),
            ("burn__compare", json!({ "models": ["one"] })),
            (
                "burn__compare",
                json!({ "models": ["one", "two"], "minSample": -1 }),
            ),
        ];

        for (name, args) in cases {
            let result = server.call_tool(name, &args).await.expect("known tool");
            assert_eq!(result.get("isError"), Some(&Value::Bool(true)), "{result}");
            assert!(result.get("structuredContent").is_none(), "{result}");
        }

        assert!(server
            .call_tool("burn__doesNotExist", &json!({}))
            .await
            .is_none());
        let recovered = server
            .call_tool("burn__summary", &json!({}))
            .await
            .expect("known tool after errors");
        assert_eq!(assert_tool_success(&recovered)["turnCount"], json!(2));
    }

    #[tokio::test]
    async fn new_tools_return_valid_shapes_on_an_empty_ledger() {
        let home = tempfile::tempdir().expect("temp ledger home");
        let handle = Ledger::open(LedgerOpenOptions::with_home(home.path()))
            .expect("open empty fixture ledger");
        let server = Server {
            handle: Arc::new(tokio::sync::Mutex::new(handle)),
            default_session_id: None,
            debug: false,
        };
        let project = home.path().join("empty-project");
        std::fs::create_dir(&project).expect("create empty project");
        let project = project.to_string_lossy();
        let calls = [
            ("burn__summary", json!({})),
            ("burn__hotspots", json!({})),
            ("burn__overhead", json!({ "project": project })),
            (
                "burn__overheadTrim",
                json!({ "project": project, "includeDiff": false }),
            ),
            ("burn__compare", json!({ "models": ["one", "two"] })),
        ];
        for (name, args) in calls {
            let result = server.call_tool(name, &args).await.expect("known tool");
            assert_tool_success(&result);
        }
    }
}
