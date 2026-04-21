import { readdir, stat } from 'node:fs/promises';
import type { Dirent } from 'node:fs';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseClaudeSession, parseOpencodeSession } from '@relayburn/reader';
import { appendTurns, loadHwm, saveHwm, type HwmMap } from '@relayburn/ledger';

const CLAUDE_PROJECTS = path.join(homedir(), '.claude', 'projects');
const OPENCODE_STORAGE = path.join(homedir(), '.local', 'share', 'opencode', 'storage');
const OPENCODE_SESSION_ROOT = path.join(OPENCODE_STORAGE, 'session');
const OPENCODE_MESSAGE_ROOT = path.join(OPENCODE_STORAGE, 'message');

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

export async function ingestOpencodeSessions(): Promise<IngestReport> {
  const hwm = await loadHwm();
  let scanned = 0;
  let ingested = 0;
  let appended = 0;

  for (const file of await walkOpencodeSessions(OPENCODE_SESSION_ROOT)) {
    scanned++;
    const sessionId = path.basename(file, '.json');
    const messageDir = path.join(OPENCODE_MESSAGE_ROOT, sessionId);
    const messageMtime = await getDirMtime(messageDir);
    if (messageMtime === null) continue;

    const prior = hwm[file];
    if (prior && prior.mtimeMs >= messageMtime) continue;

    const turns = await parseOpencodeSession(file, { sessionPath: file });
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
      mtimeMs: messageMtime,
    };
  }

  await saveHwm(hwm);
  return { scannedSessions: scanned, ingestedSessions: ingested, appendedTurns: appended };
}

export async function ingestAll(): Promise<IngestReport> {
  const a = await ingestClaudeProjects();
  const b = await ingestOpencodeSessions();
  return {
    scannedSessions: a.scannedSessions + b.scannedSessions,
    ingestedSessions: a.ingestedSessions + b.ingestedSessions,
    appendedTurns: a.appendedTurns + b.appendedTurns,
  };
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

async function walkOpencodeSessions(root: string): Promise<string[]> {
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
      else if (e.isFile() && e.name.startsWith('ses_') && e.name.endsWith('.json')) out.push(full);
    }
  }
  return out;
}

async function getDirMtime(dir: string): Promise<number | null> {
  try {
    const st = await stat(dir);
    return st.mtimeMs;
  } catch {
    return null;
  }
}

// Unused in v0; exposed for tests.
export type { HwmMap };
