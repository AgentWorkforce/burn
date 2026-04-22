import { readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import {
  parseClaudeSessionIncremental,
  parseCodexSessionIncremental,
  parseOpencodeSessionIncremental,
} from '@relayburn/reader';
import type { CodexResumeState } from '@relayburn/reader';
import {
  appendTurns,
  loadCursors,
  saveCursors,
  type ClaudeCursor,
  type CodexCursor,
  type FileCursor,
  type OpencodeCursor,
} from '@relayburn/ledger';

import { walkJsonl, walkOpencodeSessions } from './walk.js';

const CLAUDE_PROJECTS = path.join(homedir(), '.claude', 'projects');
const CODEX_SESSIONS = path.join(homedir(), '.codex', 'sessions');
const OPENCODE_STORAGE = path.join(homedir(), '.local', 'share', 'opencode', 'storage');
const OPENCODE_SESSION_ROOT = path.join(OPENCODE_STORAGE, 'session');
const OPENCODE_MESSAGE_ROOT = path.join(OPENCODE_STORAGE, 'message');

export interface IngestReport {
  scannedSessions: number;
  ingestedSessions: number;
  appendedTurns: number;
}

export async function ingestClaudeProjects(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  await ingestClaudeInto(cursors, report);
  await saveCursors(cursors);
  return report;
}

export async function ingestCodexSessions(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  await ingestCodexInto(cursors, report);
  await saveCursors(cursors);
  return report;
}

export async function ingestOpencodeSessions(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  await ingestOpencodeInto(cursors, report);
  await saveCursors(cursors);
  return report;
}

export async function ingestAll(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  await ingestClaudeInto(cursors, report);
  await ingestCodexInto(cursors, report);
  await ingestOpencodeInto(cursors, report);
  await saveCursors(cursors);
  return report;
}

async function ingestClaudeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
): Promise<void> {
  const projects = await listDirs(CLAUDE_PROJECTS);
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      report.scannedSessions++;
      try {
        const st = await stat(file);
        const prior = cursors[file];
        const priorClaude = prior?.kind === 'claude' ? prior : undefined;
        const rotated =
          !priorClaude ||
          priorClaude.inode !== st.ino ||
          st.mtimeMs < priorClaude.mtimeMs ||
          st.size < priorClaude.offsetBytes;
        const startOffset = rotated ? 0 : priorClaude.offsetBytes;

        if (!rotated && startOffset >= st.size) {
          // nothing new; refresh mtime bookkeeping
          priorClaude.mtimeMs = st.mtimeMs;
          continue;
        }

        const { turns, endOffset } = await parseClaudeSessionIncremental(file, {
          startOffset,
          sessionPath: file,
        });
        if (turns.length > 0) {
          await appendTurns(turns);
          report.appendedTurns += turns.length;
          report.ingestedSessions++;
        }
        const next: ClaudeCursor = {
          kind: 'claude',
          inode: st.ino,
          offsetBytes: endOffset,
          mtimeMs: st.mtimeMs,
        };
        cursors[file] = next;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
      }
    }
  }
}

async function ingestCodexInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
): Promise<void> {
  for (const file of await walkJsonl(CODEX_SESSIONS)) {
    report.scannedSessions++;
    try {
      const st = await stat(file);
      const prior = cursors[file];
      const priorCodex = prior?.kind === 'codex' ? prior : undefined;
      const rotated =
        !priorCodex ||
        priorCodex.inode !== st.ino ||
        st.mtimeMs < priorCodex.mtimeMs ||
        st.size < priorCodex.offsetBytes;
      const startOffset = rotated ? 0 : priorCodex.offsetBytes;
      const resume: CodexResumeState | undefined = rotated
        ? undefined
        : {
            cumulative: { ...priorCodex.cumulative },
            sessionId: priorCodex.sessionId,
            turnContexts: { ...priorCodex.turnContexts },
            ...(priorCodex.sessionCwd !== undefined ? { sessionCwd: priorCodex.sessionCwd } : {}),
          };

      if (!rotated && startOffset >= st.size) {
        priorCodex.mtimeMs = st.mtimeMs;
        continue;
      }

      const opts: Parameters<typeof parseCodexSessionIncremental>[1] = {
        startOffset,
        sessionPath: file,
      };
      if (resume !== undefined) opts.resume = resume;
      const { turns, endOffset, resume: nextResume } = await parseCodexSessionIncremental(
        file,
        opts,
      );
      if (turns.length > 0) {
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      const next: CodexCursor = {
        kind: 'codex',
        inode: st.ino,
        offsetBytes: endOffset,
        mtimeMs: st.mtimeMs,
        cumulative: nextResume.cumulative,
        sessionId: nextResume.sessionId,
        turnContexts: nextResume.turnContexts,
      };
      if (nextResume.sessionCwd !== undefined) next.sessionCwd = nextResume.sessionCwd;
      cursors[file] = next;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
    }
  }
}

async function ingestOpencodeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
): Promise<void> {
  for (const file of await walkOpencodeSessions(OPENCODE_SESSION_ROOT)) {
    report.scannedSessions++;
    try {
      const sessionId = path.basename(file, '.json');
      const messageDir = path.join(OPENCODE_MESSAGE_ROOT, sessionId);
      const messageMtime = await getDirMtime(messageDir);
      if (messageMtime === null) continue;

      const st = await stat(file);
      const prior = cursors[file];
      const priorOpencode = prior?.kind === 'opencode' ? prior : undefined;
      const rotated =
        !priorOpencode || priorOpencode.inode !== st.ino || messageMtime < priorOpencode.mtimeMs;
      const seenMessageIds = rotated
        ? new Set<string>()
        : new Set(priorOpencode.seenMessageIds);

      if (!rotated && messageMtime === priorOpencode.mtimeMs) {
        // nothing new
        continue;
      }

      const { turns, seenMessageIds: nextSeen } = await parseOpencodeSessionIncremental(file, {
        sessionPath: file,
        seenMessageIds,
      });
      if (turns.length > 0) {
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      const next: OpencodeCursor = {
        kind: 'opencode',
        inode: st.ino,
        mtimeMs: messageMtime,
        seenMessageIds: [...nextSeen],
      };
      cursors[file] = next;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
    }
  }
}

function emptyReport(): IngestReport {
  return { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };
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

async function getDirMtime(dir: string): Promise<number | null> {
  try {
    const s = await stat(dir);
    return s.mtimeMs;
  } catch {
    return null;
  }
}
