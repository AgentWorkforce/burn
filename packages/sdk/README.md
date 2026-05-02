# @relayburn/sdk

Embeddable Relayburn SDK for in-process ingestion and analysis. This package is the **source of truth** for the in-process query/compute surface — `@relayburn/mcp` and `@relayburn/cli` consume the SDK rather than duplicating its logic.

```ts
import { Ledger, ingest, summary, sessionCost, hotspots } from '@relayburn/sdk';

await Ledger.open({ home: '/tmp/relayburn-home' });
await ingest({ ledgerHome: '/tmp/relayburn-home' });

// Slice-wide rollup: turnCount + per-model + per-tool aggregates.
const stats = await summary({ session: 'session-id', ledgerHome: '/tmp/relayburn-home' });

// Compact session-scoped cost shape (totalUSD/totalTokens/turnCount/models).
// Powers the MCP `burn__sessionCost` tool.
const cost = await sessionCost({ session: 'session-id' });

const findings = await hotspots({ session: 'session-id', patterns: ['retry-loop'] });
```

`summary` and `sessionCost` read through the SQLite archive when available, transparently falling back to the JSONL ledger walk if the archive can't be opened. Pass `onLog` to surface fallback messages in your host's log channel.
