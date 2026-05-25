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
//! Tool surface today: `burn__sessionCost` (compact session cost shape)
//! and `burn__fingerprint` (cheap polling primitive — see #440). Both
//! are thin SDK wrappers, mirroring `packages/mcp/src/tools/*.ts` 1:1.
//! Other tools (`summary`, `hotspots`, …) are tracked as follow-ups so
//! the scope of D8 stays tight.

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use relayburn_sdk::{
    FingerprintScope, Ledger, LedgerHandle, LedgerOpenOptions, SessionCostOptions,
    SessionCostResult,
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
            write_response(&error_envelope(&Value::Null, -32600, "invalid request", None));
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
        let tools = json!([
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
            }
        ]);
        write_success(id, json!({ "tools": tools }));
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
        match name {
            "burn__sessionCost" => self.tool_session_cost(id, &args).await,
            "burn__fingerprint" => self.tool_fingerprint(id, &args).await,
            other => {
                write_response(&error_envelope(
                    id,
                    -32601,
                    &format!("unknown tool: {other}"),
                    None,
                ));
            }
        }
    }

    async fn tool_fingerprint(&self, id: &Value, args: &Value) {
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
                write_success(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": "fingerprint: pass at most one of sessionId / project",
                        }],
                        "isError": true,
                    }),
                );
                return;
            }
            (Some(s), None) => FingerprintScope::Session(s),
            (None, Some(p)) => FingerprintScope::Project(p),
            (None, None) => FingerprintScope::AllSessions,
        };

        let handle_guard = self.handle.lock().await;
        let result = handle_guard.fingerprint(scope);
        drop(handle_guard);

        let fp = match result {
            Ok(fp) => fp,
            Err(err) => {
                write_success(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": err.to_string() }],
                        "isError": true,
                    }),
                );
                return;
            }
        };

        let payload = json!({ "fingerprint": fp.as_str() });
        let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        write_success(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": payload,
            }),
        );
    }

    async fn tool_session_cost(&self, id: &Value, args: &Value) {
        let override_id = args
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let session = override_id
            .clone()
            .or_else(|| self.default_session_id.clone());

        // Run the SDK call. `session_cost` is sync but we already hold a
        // ledger handle — call directly on it via
        // `LedgerHandle::session_cost` so we don't re-open the ledger
        // every call. The free `relayburn_sdk::session_cost` would
        // open + close per call, which is wasteful for a long-lived
        // server.
        //
        // This branch is intentionally cheap: the entire body is CPU /
        // SQLite work, so the `await` below only yields if the global
        // ledger lock is contended (it isn't — we're the only user).
        let opts = SessionCostOptions {
            session: session.clone(),
            ledger_home: None,
        };
        let handle_guard = self.handle.lock().await;
        let result = handle_guard.session_cost(opts);
        drop(handle_guard);

        let mut payload: SessionCostResult = match result {
            Ok(r) => r,
            Err(err) => {
                let msg = err.to_string();
                // Per MCP convention: tool errors are non-throwing
                // results with `isError: true`. Reserve JSON-RPC errors
                // for protocol problems (parse / method-not-found).
                write_success(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": msg }],
                        "isError": true,
                    }),
                );
                return;
            }
        };

        // Mirror TS: when no override and no registered default, surface
        // a more descriptive note than the SDK's generic one.
        if payload.session_id.is_none() && override_id.is_none() && self.default_session_id.is_none()
        {
            payload.note = Some(
                "no session id provided and server was not registered with one".to_string(),
            );
        }

        let value = serde_json::to_value(&payload).unwrap_or(Value::Null);
        let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
        write_success(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": value,
            }),
        );
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
}
