import { readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseClaudeSession } from '@relayburn/reader';
import { appendTurns, loadHwm, saveHwm, type HwmMap } from '@relayburn/ledger';

const CLAUDE_PROJECTS = path.join(homedir(), '.claude', 'projects');

export interface IngestReport {
  scannedSessions: number;
  ingestedSessions: number;
  appendedTurns: number;
}

export async function ingestClaudeProjects(): Promise<IngestReport> {
  const hwm = await loadHwm();
  let scanned = 0;
  let ingested = 0;
  let appended = 0;

  const projects = await listDirs(CLAUDE_PROJECTS);
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      scanned++;
      const st = await stat(file);
      const prior = hwm[file];
      if (prior && prior.mtimeMs >= st.mtimeMs) continue;

      const turns = await parseClaudeSession(file, { sessionPath: file });
      if (turns.length === 0) continue;

      const newTurns = prior
        ? turns.filter((t) => t.ts > prior.lastTs || (t.ts === prior.lastTs && t.messageId !== prior.lastMessageId))
        : turns;

      if (newTurns.length > 0) {
        await appendTurns(newTurns);
        appended += newTurns.length;
        ingested++;
      }

      const last = turns[turns.length - 1]!;
      hwm[file] = {
        lastMessageId: last.messageId,
        lastTs: last.ts,
        mtimeMs: st.mtimeMs,
      };
    }
  }

  await saveHwm(hwm);
  return { scannedSessions: scanned, ingestedSessions: ingested, appendedTurns: appended };
}

async function listDirs(parent: string): Promise<string[]> {
  try {
    const entries = await readdir(parent, { withFileTypes: true });
    return entries.filter((e) => e.isDirectory()).map((e) => path.join(parent, e.name));
  } catch {
    return [];
  }
}

async function listJsonlFiles(dir: string): Promise<string[]> {
  try {
    const entries = await readdir(dir, { withFileTypes: true });
    return entries
      .filter((e) => e.isFile() && e.name.endsWith('.jsonl'))
      .map((e) => path.join(dir, e.name));
  } catch {
    return [];
  }
}

// Unused in v0; exposed for tests.
export type { HwmMap };
