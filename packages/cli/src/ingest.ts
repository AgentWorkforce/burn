import { readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import {
  parseClaudeSessionIncremental,
  parseCodexSessionIncremental,
  parseOpencodeSessionIncremental,
} from '@relayburn/reader';
import type { CodexResumeState, ContentStoreMode } from '@relayburn/reader';
import {
  appendCompactions,
  appendContent,
  appendTurns,
  listContentSessionIds,
  loadConfig,
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
  const contentMode = await resolveContentMode();
  await ingestClaudeInto(cursors, report, contentMode);
  await saveCursors(cursors);
  return report;
}

export async function ingestCodexSessions(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  await ingestCodexInto(cursors, report, contentMode);
  await saveCursors(cursors);
  return report;
}

export async function ingestOpencodeSessions(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  await ingestOpencodeInto(cursors, report, contentMode);
  await saveCursors(cursors);
  return report;
}

export async function ingestAll(): Promise<IngestReport> {
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  await ingestClaudeInto(cursors, report, contentMode);
  await ingestCodexInto(cursors, report, contentMode);
  await ingestOpencodeInto(cursors, report, contentMode);
  await saveCursors(cursors);
  return report;
}

async function resolveContentMode(): Promise<ContentStoreMode> {
  const cfg = await loadConfig();
  return cfg.content.store;
}

async function ingestClaudeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
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

        const parseOpts: Parameters<typeof parseClaudeSessionIncremental>[1] = {
          startOffset,
          sessionPath: file,
          contentMode,
        };
        const priorUserText = rotated ? undefined : priorClaude?.lastUserText;
        if (priorUserText) parseOpts.lastUserText = priorUserText;
        const { turns, content, events, endOffset, lastUserText } =
          await parseClaudeSessionIncremental(file, parseOpts);
        if (turns.length > 0) {
          await appendTurns(turns);
          report.appendedTurns += turns.length;
          report.ingestedSessions++;
        }
        if (content.length > 0) {
          await appendContent(content);
        }
        if (events.length > 0) {
          await appendCompactions(events);
        }
        const next: ClaudeCursor = {
          kind: 'claude',
          inode: st.ino,
          offsetBytes: endOffset,
          mtimeMs: st.mtimeMs,
        };
        if (lastUserText) next.lastUserText = lastUserText;
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
  contentMode: ContentStoreMode,
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
        contentMode,
      };
      if (resume !== undefined) opts.resume = resume;
      const { turns, content, endOffset, resume: nextResume } =
        await parseCodexSessionIncremental(file, opts);
      if (turns.length > 0) {
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      if (content.length > 0) {
        await appendContent(content);
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
  contentMode: ContentStoreMode,
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

      const { turns, content, seenMessageIds: nextSeen } =
        await parseOpencodeSessionIncremental(file, {
          sessionPath: file,
          seenMessageIds,
          contentMode,
        });
      if (turns.length > 0) {
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      if (content.length > 0) {
        await appendContent(content);
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

export interface ReingestContentReport {
  scannedFiles: number;
  skippedExisting: number;
  reingestedSessions: number;
  appendedContent: number;
  failed: number;
}

// Re-parse source session files to populate missing content sidecars. Used by
// `burn rebuild --content` to fix up historical sessions ingested before the
// sidecar was written (or where the sidecar was pruned). Does NOT touch
// cursors, ledger turns, or compactions — only writes content records for
// sessions that currently have no sidecar on disk.
export async function reingestMissingContent(): Promise<ReingestContentReport> {
  const existing = await listContentSessionIds();
  const report: ReingestContentReport = {
    scannedFiles: 0,
    skippedExisting: 0,
    reingestedSessions: 0,
    appendedContent: 0,
    failed: 0,
  };
  await reingestClaudeContent(existing, report);
  await reingestCodexContent(existing, report);
  await reingestOpencodeContent(existing, report);
  return report;
}

async function reingestClaudeContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  const projects = await listDirs(CLAUDE_PROJECTS);
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      report.scannedFiles++;
      const sessionId = path.basename(file, '.jsonl');
      if (existing.has(sessionId)) {
        report.skippedExisting++;
        continue;
      }
      try {
        const { content } = await parseClaudeSessionIncremental(file, {
          startOffset: 0,
          sessionPath: file,
          contentMode: 'full',
        });
        const filtered = content.filter((c) => !existing.has(c.sessionId));
        if (filtered.length > 0) {
          await appendContent(filtered);
          report.appendedContent += filtered.length;
          report.reingestedSessions++;
          for (const c of filtered) existing.add(c.sessionId);
        }
      } catch (err) {
        report.failed++;
        const msg = err instanceof Error ? err.message : String(err);
        process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
      }
    }
  }
}

async function reingestCodexContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkJsonl(CODEX_SESSIONS)) {
    report.scannedFiles++;
    const derived = deriveCodexSessionId(file);
    if (derived && existing.has(derived)) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content } = await parseCodexSessionIncremental(file, {
        startOffset: 0,
        sessionPath: file,
        contentMode: 'full',
      });
      const filtered = content.filter((c) => !existing.has(c.sessionId));
      if (filtered.length > 0) {
        await appendContent(filtered);
        report.appendedContent += filtered.length;
        report.reingestedSessions++;
        for (const c of filtered) existing.add(c.sessionId);
      }
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

async function reingestOpencodeContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkOpencodeSessions(OPENCODE_SESSION_ROOT)) {
    report.scannedFiles++;
    const sessionId = path.basename(file, '.json');
    if (existing.has(sessionId)) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content } = await parseOpencodeSessionIncremental(file, {
        sessionPath: file,
        seenMessageIds: new Set<string>(),
        contentMode: 'full',
      });
      const filtered = content.filter((c) => !existing.has(c.sessionId));
      if (filtered.length > 0) {
        await appendContent(filtered);
        report.appendedContent += filtered.length;
        report.reingestedSessions++;
        for (const c of filtered) existing.add(c.sessionId);
      }
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

// Codex filenames are `rollout-<timestamp>-<uuid>.jsonl` where the UUID is the
// session id. Extract it for a cheap skip check before parsing. If the pattern
// doesn't match, return null and fall back to post-filtering.
function deriveCodexSessionId(file: string): string | null {
  const base = path.basename(file, '.jsonl');
  const m = base.match(
    /([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/,
  );
  return m ? m[1]! : null;
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
