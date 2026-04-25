import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';

import { parseClaudeSession } from '@relayburn/reader';
import {
  appendContent,
  appendTurns,
  loadConfig,
  loadCursors,
  saveCursors,
  stamp,
} from '@relayburn/ledger';
import type { ClaudeCursor, Enrichment } from '@relayburn/ledger';
import { homedir } from 'node:os';
import * as path from 'node:path';
import { stat } from 'node:fs/promises';

import type { ParsedArgs } from '../args.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';

export async function runClaudeWrapper(args: ParsedArgs): Promise<number> {
  const sessionId = randomUUID();
  const passthrough = args.passthrough;
  const claudeArgs = ['--session-id', sessionId, ...passthrough];

  const envTags = readSpawnEnvTags();
  const tags: Enrichment = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = 'claude';
  tags['burnSpawn'] = '1';
  tags['burnSpawnTs'] = new Date().toISOString();

  await stamp({ sessionId }, tags);

  process.stderr.write(`[burn] session-id=${sessionId}\n`);

  const cwd = process.cwd();
  const child = spawn('claude', claudeArgs, {
    stdio: 'inherit',
    env: {
      ...process.env,
      ...spawnTagEnvOverrides(tags),
      RELAYBURN_SESSION_ID: sessionId,
    },
  });

  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn claude: ${err.message}\n`);
      resolve(127);
    });
  });

  await ingestSession(cwd, sessionId);
  return code;
}

export async function ingestSession(cwd: string, sessionId: string): Promise<void> {
  const encoded = cwd.replace(/\//g, '-');
  const file = path.join(homedir(), '.claude', 'projects', encoded, `${sessionId}.jsonl`);
  let st: Awaited<ReturnType<typeof stat>>;
  try {
    st = await stat(file);
    if (!st.isFile()) return;
  } catch {
    process.stderr.write(`[burn] no session file found at ${file}\n`);
    return;
  }
  const cfg = await loadConfig();
  const { turns, content } = await parseClaudeSession(file, {
    sessionPath: file,
    contentMode: cfg.content.store,
  });
  if (turns.length === 0) return;
  await appendTurns(turns);
  if (content.length > 0) await appendContent(content);

  // Persist a cursor so a later `burn summary` (which calls ingestAll) skips
  // this file instead of re-parsing and re-appending its content. Turns are
  // protected by appendTurns dedup, but appendContent has no dedup — without
  // this, content records would duplicate on every subsequent invocation.
  const cursors = await loadCursors();
  const cursor: ClaudeCursor = {
    kind: 'claude',
    inode: st.ino,
    offsetBytes: st.size,
    mtimeMs: st.mtimeMs,
  };
  cursors[file] = cursor;
  await saveCursors(cursors);

  process.stderr.write(`[burn] ingested ${turns.length} turns from ${file}\n`);
}
