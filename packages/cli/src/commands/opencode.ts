import { spawn } from 'node:child_process';
import { stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseOpencodeSession } from '@relayburn/reader';
import { appendContent, appendTurns, loadConfig, stamp } from '@relayburn/ledger';
import type { Enrichment } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';
import { walkOpencodeSessions } from '../walk.js';

const OPENCODE_STORAGE = path.join(homedir(), '.local', 'share', 'opencode', 'storage');
const OPENCODE_SESSION_ROOT = path.join(OPENCODE_STORAGE, 'session');

export async function runOpencodeWrapper(args: ParsedArgs): Promise<number> {
  const envTags = readSpawnEnvTags();
  const tags: Enrichment = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = 'opencode';
  tags['burnSpawn'] = '1';
  const spawnStartTs = Date.now();
  tags['burnSpawnTs'] = new Date(spawnStartTs).toISOString();

  const preSnapshot = await snapshotSessionFiles();
  process.stderr.write(`[burn] opencode spawn: tracking ${preSnapshot.size} existing sessions\n`);

  const child = spawn('opencode', args.passthrough, {
    stdio: 'inherit',
    env: { ...process.env, ...spawnTagEnvOverrides(tags) },
  });
  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn opencode: ${err.message}\n`);
      resolve(127);
    });
  });

  const newFiles = await findNewSessionFiles(preSnapshot, spawnStartTs);
  if (newFiles.length === 0) {
    process.stderr.write(`[burn] no new opencode session files found under ${OPENCODE_SESSION_ROOT}\n`);
    return code;
  }

  const cfg = await loadConfig();
  for (const file of newFiles) {
    const { turns, content } = await parseOpencodeSession(file, {
      sessionPath: file,
      contentMode: cfg.content.store,
    });
    if (turns.length === 0) continue;
    await appendTurns(turns);
    if (content.length > 0) await appendContent(content);
    const sessionId = turns[0]!.sessionId;
    if (sessionId) await stamp({ sessionId }, tags);
    process.stderr.write(`[burn] ingested ${turns.length} turns from ${file}\n`);
  }

  return code;
}

async function snapshotSessionFiles(): Promise<Set<string>> {
  const out = new Set<string>();
  for (const file of await walkOpencodeSessions(OPENCODE_SESSION_ROOT)) out.add(file);
  return out;
}

async function findNewSessionFiles(pre: Set<string>, spawnStartTs: number): Promise<string[]> {
  const now = await walkOpencodeSessions(OPENCODE_SESSION_ROOT);
  const candidates: string[] = [];
  for (const file of now) {
    if (pre.has(file)) continue;
    try {
      const st = await stat(file);
      if (st.mtimeMs + 1 < spawnStartTs) continue;
      candidates.push(file);
    } catch {
      continue;
    }
  }
  return candidates;
}
