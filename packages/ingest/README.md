# @relayburn/ingest

Session-store discovery, parse-and-append orchestration, pending-stamp
resolution, and watch-loop primitives for relayburn. Powers `burn ingest`,
`burn run <harness>`, and the `@relayburn/sdk` `ingest()` entrypoint.

```ts
import {
  ingestAll,
  ingestClaudeProjects,
  ingestCodexSessions,
  ingestOpencodeSessions,
  startWatchLoop,
} from '@relayburn/ingest';

// One-shot scan of every supported session store.
const report = await ingestAll();

// Polling watcher (drains stores on an interval).
const controller = startWatchLoop({ intervalMs: 1000 });
// ...
await controller.stop();
```

Depends on `@relayburn/reader` and `@relayburn/ledger`. Has no dependency on
`@relayburn/cli` or `@relayburn/sdk`.
