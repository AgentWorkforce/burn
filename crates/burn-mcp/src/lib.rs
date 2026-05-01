//! `burn-mcp` — stdio MCP server exposing read-only ledger queries.
//!
//! Mirrors `packages/mcp/src/` from the TypeScript workspace. Planned
//! modules (filed as sub-issues under #222):
//!
//! - `server`               — stdio MCP transport + request dispatch
//! - `config`               — server configuration
//! - `types`                — request/response shapes
//! - `tools::current_block` — `burn_current_block` tool
//! - `tools::session_cost`  — `burn_session_cost` tool
//! - `tools::archive_backed` — archive-backed query tools
//!
//! Open question (#210): whether this crate is a standalone server or a
//! thin shell over `burn --json`. Resolved during the burn-mcp port.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {}
}
