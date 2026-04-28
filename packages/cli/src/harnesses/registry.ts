import { claudeAdapter } from './claude.js';
import { codexAdapter } from './codex.js';
import { opencodeAdapter } from './opencode.js';
import type { HarnessAdapter } from './types.js';

// Static registry of known harnesses. Adding a fourth harness is a one-line
// import + one-line array entry — no driver changes, no help-block edits.
const ADAPTERS: ReadonlyArray<HarnessAdapter> = [
  claudeAdapter,
  codexAdapter,
  opencodeAdapter,
];

export function lookupHarness(name: string): HarnessAdapter | undefined {
  return ADAPTERS.find((a) => a.name === name);
}

export function listHarnessNames(): string[] {
  return ADAPTERS.map((a) => a.name);
}
