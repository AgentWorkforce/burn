// Seals a Node test process away from the user's real session stores.
//
// Loaded with `node --import` before any test (and inherited by the test
// runner's child processes): `HOME` and `USERPROFILE` point at a tripwire
// — a regular file — and every variable that relocates a store is removed,
// so a default the native SDK resolves (the ai-hist database, the burn
// ledger, `~/.claude`, `~/.codex`, OpenCode) lands beneath a file and fails
// with `Not a directory` instead of touching real data. Mirrors
// crates/relayburn-test-seal.

import { closeSync, mkdirSync, openSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const RELOCATING_VARS = new Set([
  'CLAUDE_CONFIG_DIR',
  'CODEX_HOME',
  'GROK_HOME',
  'HERMES_HOME',
  'OPENCODE_DB',
  'OPENCODE_STORAGE_DIR',
  'PI_CODING_AGENT_DIR',
  'PI_CODING_AGENT_SESSION_DIR',
  'RELAYBURN_CONTENT_PATH',
  'RELAYBURN_HOME',
  'RELAYBURN_SQLITE_PATH',
]);
const RELOCATING_PREFIXES = ['AI_HIST_', 'XDG_'];

export const TRIPWIRE = join(tmpdir(), 'relayburn-test-seal', 'no-home-in-tests');

mkdirSync(join(tmpdir(), 'relayburn-test-seal'), { recursive: true });
closeSync(openSync(TRIPWIRE, 'a'));
if (!statSync(TRIPWIRE).isFile()) {
  throw new Error(`${TRIPWIRE} must be a regular file so nothing can be created beneath it`);
}

for (const key of Object.keys(process.env)) {
  if (RELOCATING_VARS.has(key) || RELOCATING_PREFIXES.some((p) => key.startsWith(p))) {
    delete process.env[key];
  }
}
process.env.HOME = TRIPWIRE;
process.env.USERPROFILE = TRIPWIRE;
