import { spawn } from 'node:child_process';
import { homedir } from 'node:os';
import * as path from 'node:path';

import type { Enrichment } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import { ingestCodexSessions } from '../ingest.js';
import { writePendingStamp } from '../pending-stamps.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';
import { startWatchLoop } from './watch.js';

function codexSessionsDir(): string {
  return path.join(homedir(), '.codex', 'sessions');
}

export async function runCodexWrapper(args: ParsedArgs): Promise<number> {
  const envTags = readSpawnEnvTags();
  const tags: Enrichment = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = 'codex';
  tags['burnSpawn'] = '1';
  const spawnStartTs = new Date();
  tags['burnSpawnTs'] = spawnStartTs.toISOString();

  const pending = await writePendingStamp({
    harness: 'codex',
    cwd: process.cwd(),
    enrichment: tags,
    sessionDirHint: codexSessionsDir(),
    spawnStartTs,
  });
  process.stderr.write(`[burn] codex spawn: pending stamp ${path.basename(pending.file)}\n`);

  const watcher = startWatchLoop({ immediate: false });

  const child = spawn('codex', args.passthrough, {
    stdio: 'inherit',
    env: { ...process.env, ...spawnTagEnvOverrides(tags) },
  });
  void watcher.tick();
  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn codex: ${err.message}\n`);
      resolve(127);
    });
  });

  await watcher.stop();
  const report = await ingestCodexSessions();
  process.stderr.write(
    `[burn] codex ingest: ${report.ingestedSessions} session` +
      `${report.ingestedSessions === 1 ? '' : 's'} ` +
      `(+${report.appendedTurns} turn${report.appendedTurns === 1 ? '' : 's'})\n`,
  );

  return code;
}
