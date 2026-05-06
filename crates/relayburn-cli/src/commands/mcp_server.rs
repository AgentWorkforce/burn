//! `burn mcp-server` — stdio MCP server exposing read-only ledger
//! queries for in-session self-query (closes #210).
//!
//! Stub. Wave 2 D8 wires this up via `rmcp` around the SDK's read-only
//! query verbs (`session_cost`, `summary`, `hotspots`, …). TS source
//! of truth: `packages/cli/src/commands/mcp-server.ts`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("mcp-server", globals)
}
