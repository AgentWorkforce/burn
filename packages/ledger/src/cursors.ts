import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import * as path from 'node:path';

import type {
  CodexLastCompletedTurn,
  OpencodeStreamCursorState,
  PersistedUserTurnSlot,
} from '@relayburn/reader';

import { withLock } from './lock.js';
import { cursorsPath } from './paths.js';

export interface ClaudeCursor {
  kind: 'claude';
  inode: number;
  offsetBytes: number;
  mtimeMs: number;
  // The last user prompt text as of `offsetBytes`. Carried across calls so
  // the activity classifier keeps keyword context when `offsetBytes` backed
  // up past a user message to defer an incomplete assistant turn.
  lastUserText?: string;
}

export interface CodexCursor {
  kind: 'codex';
  inode: number;
  offsetBytes: number;
  mtimeMs: number;
  cumulative: { input: number; output: number; cacheRead: number; reasoning: number };
  sessionId: string;
  sessionCwd?: string;
  turnContexts: Record<string, { turn_id?: string; cwd?: string; model?: string }>;
  userTurnSlot?: PersistedUserTurnSlot;
  rootSessionEmitted?: boolean;
  nextEventIndex?: number;
  toolResultCounters?: Record<string, number>;
  lastCompletedTurn?: CodexLastCompletedTurn;
}

export interface OpencodeCursor {
  kind: 'opencode';
  inode: number;
  mtimeMs: number;
  seenMessageIds: string[];
}

export interface OpencodeStreamCursor extends OpencodeStreamCursorState {
  kind: 'opencode-stream';
}

export type FileCursor = ClaudeCursor | CodexCursor | OpencodeCursor | OpencodeStreamCursor;

interface CursorsFile {
  files: Record<string, FileCursor>;
}

export async function loadCursors(): Promise<Record<string, FileCursor>> {
  try {
    const raw = await readFile(cursorsPath(), 'utf8');
    const parsed = JSON.parse(raw) as CursorsFile;
    if (parsed && typeof parsed === 'object' && parsed.files && typeof parsed.files === 'object') {
      return parsed.files;
    }
  } catch {
    // missing or malformed: treat as empty
  }
  return {};
}

export async function saveCursors(map: Record<string, FileCursor>): Promise<void> {
  const finalPath = cursorsPath();
  await mkdir(path.dirname(finalPath), { recursive: true });
  const payload: CursorsFile = { files: map };
  const tmpPath = `${finalPath}.tmp`;
  await withLock('cursors', async () => {
    await writeFile(tmpPath, JSON.stringify(payload, null, 2), 'utf8');
    await rename(tmpPath, finalPath);
  });
}

export async function updateCursors(
  mutate: (map: Record<string, FileCursor>) => void | Promise<void>,
): Promise<void> {
  const finalPath = cursorsPath();
  await mkdir(path.dirname(finalPath), { recursive: true });
  const tmpPath = `${finalPath}.tmp`;
  await withLock('cursors', async () => {
    let map: Record<string, FileCursor> = {};
    try {
      const raw = await readFile(finalPath, 'utf8');
      const parsed = JSON.parse(raw) as CursorsFile;
      if (parsed && typeof parsed === 'object' && parsed.files && typeof parsed.files === 'object') {
        map = parsed.files;
      }
    } catch {
      map = {};
    }
    await mutate(map);
    const payload: CursorsFile = { files: map };
    await writeFile(tmpPath, JSON.stringify(payload, null, 2), 'utf8');
    await rename(tmpPath, finalPath);
  });
}
