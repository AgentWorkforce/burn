import { spawn } from 'node:child_process';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseCodexSession } from '@relayburn/reader';
import {
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  loadConfig,
  stamp,
} from '@relayburn/ledger';
import type { Enrichment } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';
import { walkJsonl } from '../walk.js';

const CODEX_SESSIONS = path.join(homedir(), '.codex', 'sessions');

export async function runCodexWrapper(args: ParsedArgs): Promise<number> {
  const envTags = readSpawnEnvTags();
  const tags: Enrichment = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = 'codex';
  tags['burnSpawn'] = '1';
  const spawnStartTs = Date.now();
  tags['burnSpawnTs'] = new Date(spawnStartTs).toISOString();

  const preSnapshot = await snapshotSessions();
  process.stderr.write(`[burn] codex spawn: tracking ${preSnapshot.size} existing sessions\n`);

  const child = spawn('codex', args.passthrough, {
    stdio: 'inherit',
    env: { ...process.env, ...spawnTagEnvOverrides(tags) },
  });
  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn codex: ${err.message}\n`);
      resolve(127);
    });
  });

  const newFiles = await findNewSessions(preSnapshot);
  if (newFiles.length === 0) {
    process.stderr.write(`[burn] no new codex session files found under ${CODEX_SESSIONS}\n`);
    return code;
  }

  const cfg = await loadConfig();
  for (const file of newFiles) {
    const { turns, content, relationships, toolResultEvents, userTurns } = await parseCodexSession(
      file,
      {
        sessionPath: file,
        contentMode: cfg.content.store,
      },
    );
    if (turns.length === 0) continue;
    await appendTurns(turns);
    if (content.length > 0) await appendContent(content);
    if (relationships.length > 0) await appendRelationships(relationships);
    if (toolResultEvents.length > 0) await appendToolResultEvents(toolResultEvents);
    if (userTurns.length > 0) await appendUserTurns(userTurns);
    const sessionId = turns[0]!.sessionId;
    if (sessionId) await stamp({ sessionId }, tags);
    process.stderr.write(`[burn] ingested ${turns.length} turns from ${file}\n`);
  }

  return code;
}

async function snapshotSessions(): Promise<Set<string>> {
  const out = new Set<string>();
  for (const file of await walkJsonl(CODEX_SESSIONS)) out.add(file);
  return out;
}

async function findNewSessions(pre: Set<string>): Promise<string[]> {
  const now = await walkJsonl(CODEX_SESSIONS);
  return now.filter((file) => !pre.has(file));
}
