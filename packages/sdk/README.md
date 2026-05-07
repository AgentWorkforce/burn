 **Deprecated.** `@relayburn/sdk@1.x` is in maintenance-only mode. New work lands in `@relayburn/sdk@2.x` (the napi-rs binding over the Rust `relayburn-sdk` crate). Embedders should pin `^2.0.0` once it ships. The 1.x type surface is frozen and will not gain new fields. See [#249](https://github.com/AgentWorkforce/burn/issues/249) for the cutover schedule.

# @relayburn/sdk

Embeddable Relayburn SDK for in-process ingestion and analysis. This package is the **source of truth** for the in-process query/compute surface — `@relayburn/mcp` and `@relayburn/cli` consume the SDK rather than duplicating its logic.

```ts
import {
  Ledger,
  ingest,
  summary,
  sessionCost,
  compare,
  overhead,
  overheadTrim,
  hotspots,
} from '@relayburn/sdk';

await Ledger.open({ home: '/tmp/relayburn-home' });
await ingest({ ledgerHome: '/tmp/relayburn-home' });

// Slice-wide rollup: turnCount + per-model + per-tool aggregates.
const stats = await summary({ session: 'session-id', ledgerHome: '/tmp/relayburn-home' });

// Compact session-scoped cost shape (totalUSD/totalTokens/turnCount/models).
// Powers the MCP `burn__sessionCost` tool.
const cost = await sessionCost({ session: 'session-id' });

// Per-(model, activity) comparison shape — the JSON object `burn compare --json` emits.
const cmp = await compare({
  models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
  since: '30d',
  minFidelity: 'usage-only',
});

// Overhead-file (CLAUDE.md / AGENTS.md / .claude/CLAUDE.md) cost attribution.
const oh = await overhead({ project: '/path/to/repo', since: '30d' });
const trim = await overheadTrim({ project: '/path/to/repo', top: 3 });

// Per-axis hotspot attribution + pattern findings. Returns a discriminated
// union — branch on `kind`:
//   { kind: 'attribution', files, bashVerbs, bash, subagents, sessions, … }
//   { kind: 'bash' | 'bash-verb' | 'file' | 'subagent', rows: [...] }
//   { kind: 'findings', findings: WasteFinding[], summary }
const attribution = await hotspots({ session: 'session-id' });
const fileRows = await hotspots({ session: 'session-id', groupBy: 'file' });
const findings = await hotspots({ session: 'session-id', patterns: ['retry-loop'] });
```

`summary`, `sessionCost`, `compare`, `overhead`, `overheadTrim`, and `hotspots` read through the SQLite archive when available, transparently falling back to the JSONL ledger walk if the archive can't be opened. Pass `onLog` to surface fallback messages in your host's log channel.

`overheadTrim` includes a unified-diff string per recommendation by default (matches `burn overhead trim --json`); pass `includeDiff: false` to skip the per-file disk reads when you only need the recommendation rows.
