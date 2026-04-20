import { readdir, stat } from 'node:fs/promises';
import type { Dirent } from 'node:fs';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { parseClaudeSession, parseCodexSession, type TurnRecord } from '@relayburn/reader';
import { appendTurns, loadHwm, saveHwm, type HwmMap } from '@relayburn/ledger';

const CLAUDE_PROJECTS = path.join(homedir(), '.claude', 'projects');
const CODEX_SESSIONS = path.join(homedir(), '.codex', 'sessions');

export interface IngestReport {
  scannedSessions: number;
  ingestedSessions: number;
  appendedTurns: number;
}

export async function ingestClaudeProjects(): Promise<IngestReport> {
  const hwm = await loadHwm();
  const report = { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };

  const projects = await listDirs(CLAUDE_PROJECTS);
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      await ingestOne(file, hwm, report, (f) => parseClaudeSession(f, { sessionPath: f }));
    }
  }

  await saveHwm(hwm);
  return report;
}

export async function ingestCodexSessions(): Promise<IngestReport> {
  const hwm = await loadHwm();
  const report = { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };

  for (const file of await walkJsonl(CODEX_SESSIONS)) {
    await ingestOne(file, hwm, report, (f) => parseCodexSession(f, { sessionPath: f }));
  }

  await saveHwm(hwm);
  return report;
}

export async function ingestAll(): Promise<IngestReport> {
  const a = await ingestClaudeProjects();
  const b = await ingestCodexSessions();
  return {
    scannedSessions: a.scannedSessions + b.scannedSessions,
    ingestedSessions: a.ingestedSessions + b.ingestedSessions,
    appendedTurns: a.appendedTurns + b.appendedTurns,
  };
}

async function ingestOne(
  file: string,
  hwm: HwmMap,
  report: IngestReport,
  parse: (f: string) => Promise<TurnRecord[]>,
): Promise<void> {
  report.scannedSessions++;
  const st = await stat(file);
  const prior = hwm[file];
  if (prior && prior.mtimeMs >= st.mtimeMs) return;

  const turns = await parse(file);
  if (turns.length === 0) return;

  const newTurns = prior
    ? turns.filter(
        (t) => t.ts > prior.lastTs || (t.ts === prior.lastTs && t.messageId !== prior.lastMessageId),
      )
    : turns;

  if (newTurns.length > 0) {
    await appendTurns(newTurns);
    report.appendedTurns += newTurns.length;
    report.ingestedSessions++;
  }

  const last = turns[turns.length - 1]!;
  hwm[file] = {
    lastMessageId: last.messageId,
    lastTs: last.ts,
    mtimeMs: st.mtimeMs,
  };
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
