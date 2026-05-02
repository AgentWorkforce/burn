import { homedir } from 'node:os';
import * as path from 'node:path';

import { ingestOpencodeSessions } from '@relayburn/ingest';

import { createPendingStampAdapter } from './pending-stamp.js';

function opencodeSessionRoot(): string {
  return path.join(homedir(), '.local', 'share', 'opencode', 'storage', 'session');
}

export const opencodeAdapter = createPendingStampAdapter({
  name: 'opencode',
  sessionRoot: opencodeSessionRoot,
  ingestSessions: ingestOpencodeSessions,
});
