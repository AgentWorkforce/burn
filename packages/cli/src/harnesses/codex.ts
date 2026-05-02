import { homedir } from 'node:os';
import * as path from 'node:path';

import { ingestCodexSessions } from '@relayburn/ingest';

import { createPendingStampAdapter } from './pending-stamp.js';

function codexSessionsDir(): string {
  return path.join(homedir(), '.codex', 'sessions');
}

export const codexAdapter = createPendingStampAdapter({
  name: 'codex',
  sessionRoot: codexSessionsDir,
  ingestSessions: ingestCodexSessions,
});
