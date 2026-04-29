import type { HarnessAdapter } from './types.js';

type AdapterLoader = () => Promise<HarnessAdapter>;

// Static registry of known harnesses. Adapter modules are loaded lazily so
// top-level help and unrelated commands do not pay ingest/ledger startup cost.
const ADAPTERS: Record<string, AdapterLoader> = {
  claude: async () => (await import('./claude.js')).claudeAdapter,
  codex: async () => (await import('./codex.js')).codexAdapter,
  opencode: async () => (await import('./opencode.js')).opencodeAdapter,
};

export async function lookupHarness(name: string): Promise<HarnessAdapter | undefined> {
  const load = ADAPTERS[name];
  return load ? load() : undefined;
}

export function listHarnessNames(): string[] {
  return Object.keys(ADAPTERS);
}
