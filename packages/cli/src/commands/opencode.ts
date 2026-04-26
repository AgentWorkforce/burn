import { spawn } from 'node:child_process';
import { homedir } from 'node:os';
import * as path from 'node:path';

import type { Enrichment } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import { ingestOpencodeSessions } from '../ingest.js';
import { writePendingStamp } from '../pending-stamps.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';
import { startWatchLoop } from './watch.js';

function opencodeSessionRoot(): string {
  return path.join(homedir(), '.local', 'share', 'opencode', 'storage', 'session');
}

export async function runOpencodeWrapper(args: ParsedArgs): Promise<number> {
  const envTags = readSpawnEnvTags();
  const tags: Enrichment = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = 'opencode';
  tags['burnSpawn'] = '1';
  const spawnStartTs = new Date();
  tags['burnSpawnTs'] = spawnStartTs.toISOString();

  const pending = await writePendingStamp({
    harness: 'opencode',
    cwd: process.cwd(),
    enrichment: tags,
    sessionDirHint: opencodeSessionRoot(),
    spawnStartTs,
  });
  process.stderr.write(`[burn] opencode spawn: pending stamp ${path.basename(pending.file)}\n`);

  const watcher = startWatchLoop({ immediate: false });

  const child = spawn('opencode', args.passthrough, {
    stdio: 'inherit',
    env: { ...process.env, ...spawnTagEnvOverrides(tags) },
  });
  void watcher.tick();
  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn opencode: ${err.message}\n`);
      resolve(127);
    });
  });

  await watcher.stop();
  const report = await ingestOpencodeSessions();
  process.stderr.write(
    `[burn] opencode ingest: ${report.ingestedSessions} session` +
      `${report.ingestedSessions === 1 ? '' : 's'} ` +
      `(+${report.appendedTurns} turn${report.appendedTurns === 1 ? '' : 's'})\n`,
  );

  return code;
}
