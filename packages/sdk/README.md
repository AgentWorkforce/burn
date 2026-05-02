# @relayburn/sdk

Embeddable Relayburn SDK for in-process ingestion and analysis.

```ts
import { Ledger, ingest, summary, hotspots } from '@relayburn/sdk';

await Ledger.open({ home: '/tmp/relayburn-home' });
await ingest({ ledgerHome: '/tmp/relayburn-home' });
const stats = await summary({ session: 'session-id', ledgerHome: '/tmp/relayburn-home' });
const findings = await hotspots({ session: 'session-id', patterns: ['retry-loop'] });
```
