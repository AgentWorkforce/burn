import { spawn } from 'node:child_process';
import { readdir, stat } from 'node:fs/promises';
import type { Dirent } from 'node:fs';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseCodexSession } from '@relayburn/reader';
import { appendTurns, stamp } from '@relayburn/ledger';
import type { Enrichment } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';

const CODEX_SESSIONS = path.join(homedir(), '.codex', 'sessions');

export async function runCodexWrapper(args: ParsedArgs): Promise<number> {
  const tags: Enrichment = { ...args.tags };
  tags['harness'] = 'codex';
  tags['burnSpawn'] = '1';
  const spawnStartTs = Date.now();
  tags['burnSpawnTs'] = new Date(spawnStartTs).toISOString();

  const preSnapshot = await snapshotSessions();
  process.stderr.write(`[burn] codex spawn: tracking ${preSnapshot.size} existing sessions\n`);

  const child = spawn('codex', args.passthrough, { stdio: 'inherit', env: process.env });
  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err) => {
      process.stderr.write(`[burn] failed to spawn codex: ${err.message}\n`);
      resolve(127);
    });
  });

  const newFiles = await findNewSessions(preSnapshot, spawnStartTs);
  if (newFiles.length === 0) {
    process.stderr.write(`[burn] no new codex session files found under ${CODEX_SESSIONS}\n`);
    return code;
  }

  for (const file of newFiles) {
    const turns = await parseCodexSession(file, { sessionPath: file });
    if (turns.length === 0) continue;
    await appendTurns(turns);
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

async function findNewSessions(pre: Set<string>, spawnStartTs: number): Promise<string[]> {
  const now = await walkJsonl(CODEX_SESSIONS);
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

async function walkJsonl(root: string): Promise<string[]> {
  const out: string[] = [];
  const stack: string[] = [root];
  while (stack.length > 0) {
    const dir = stack.pop()!;
    let entries: Dirent[];
    try {
      entries = (await readdir(dir, { withFileTypes: true })) as Dirent[];
    } catch {
      continue;
    }
    for (const e of entries) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) stack.push(full);
      else if (e.isFile() && e.name.endsWith('.jsonl')) out.push(full);
    }
  }
  return out;
}
