import { utimes } from 'node:fs/promises';

import type { ContentRecord } from '@relayburn/reader';

import { getAdapter } from './adapters/factory.js';
import { contentFilePath } from './paths.js';

export interface PruneOptions {
  olderThanMs: number;
  // When provided, prune skips sessions for which the callback returns true -
  // the source session file still exists upstream and `burn state rebuild content`
  // can rederive the sidecar at any time, so deleting it would be silently
  // lossy. The ledger package stays decoupled from adapter-specific paths;
  // the caller (CLI) builds the source index and supplies the predicate.
  isRecoverable?: (sessionId: string) => boolean | Promise<boolean>;
}

export interface PruneResult {
  filesDeleted: number;
  bytesFreed: number;
  // Sidecars left in place because `isRecoverable(sessionId)` returned true.
  // Counted separately from `filesDeleted` so callers can surface a "kept N
  // recoverable sidecars" line and point users at `--force` if they really
  // want to reclaim that disk.
  skippedRecoverable: number;
}

export interface ReadContentSelector {
  sessionId: string;
  messageId?: string;
}

export async function appendContent(records: ContentRecord[]): Promise<void> {
  return getAdapter().appendContent(records);
}

export async function readContent(
  selector: ReadContentSelector,
): Promise<ContentRecord[]> {
  const out: ContentRecord[] = [];
  for await (const record of getAdapter().readContent(selector)) out.push(record);
  return out;
}

export async function pruneContent(options: PruneOptions): Promise<PruneResult> {
  return getAdapter().pruneContent(options);
}

export async function listContentSessionIds(): Promise<Set<string>> {
  return new Set(await getAdapter().listContentSessionIds());
}

// Test helper: set the mtime/atime on a session's content file.
export async function __setContentFileMtimeForTesting(
  sessionId: string,
  when: Date,
): Promise<void> {
  await utimes(contentFilePath(sessionId), when, when);
}
