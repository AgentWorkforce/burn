# `@relayburn/mcp`

Read-only Model Context Protocol tools over the in-process `@relayburn/sdk`
query surface. The package provides tool factories and a small stdio server;
callers choose which factories to register.

| Tool | SDK function | Purpose |
| --- | --- | --- |
| `burn__sessionCost` | `sessionCost()` | Compact cost and token totals for one session. |
| `burn__fingerprint` | `fingerprint()` | Cheap change-detection fingerprint for a ledger scope. |
| `burn__summary` | `summary()` | Cost and token totals by tool, model, or enrichment tag. |
| `burn__hotspots` | `hotspots()` | Attribution, grouped hotspots, and findings. |
| `burn__overhead` | `overhead()` | Instruction-file cost by file and section. |
| `burn__overheadTrim` | `overheadTrim()` | Ranked instruction-file trimming recommendations. |
| `burn__compare` | `compare()` | Per-model, per-activity cost and outcome comparison. |

Each factory accepts an optional injected SDK function for tests. Production
callers can omit that override and the tool calls `@relayburn/sdk` directly.
Tool results are returned as both MCP text content and unmodified structured
content by `startStdioServer()`.
