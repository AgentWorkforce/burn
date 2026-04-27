import { parseClaudeSession } from '@relayburn/reader';
import {
  appendCompactions,
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  loadConfig,
  loadCursors,
  saveCursors,
} from '@relayburn/ledger';
import type { ClaudeCursor } from '@relayburn/ledger';
import { homedir } from 'node:os';
import * as path from 'node:path';
import { stat } from 'node:fs/promises';

import type { IngestReport } from '../ingest.js';

export async function ingestSession(cwd: string, sessionId: string): Promise<IngestReport> {
  const encoded = cwd.replace(/\//g, '-');
  const file = path.join(homedir(), '.claude', 'projects', encoded, `${sessionId}.jsonl`);
  let st: Awaited<ReturnType<typeof stat>>;
  try {
    st = await stat(file);
    if (!st.isFile()) return emptyReport();
  } catch {
    process.stderr.write(`[burn] no session file found at ${file}\n`);
    return emptyReport();
  }
  const cfg = await loadConfig();
  const { turns, content, events, relationships, toolResultEvents, userTurns } =
    await parseClaudeSession(file, {
      sessionPath: file,
      contentMode: cfg.content.store,
    });
  if (turns.length === 0) return { scannedSessions: 1, ingestedSessions: 0, appendedTurns: 0 };
  await appendTurns(turns);
  if (content.length > 0) await appendContent(content);
  if (events.length > 0) await appendCompactions(events);
  if (relationships.length > 0) await appendRelationships(relationships);
  if (toolResultEvents.length > 0) await appendToolResultEvents(toolResultEvents);
  if (userTurns.length > 0) await appendUserTurns(userTurns);

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

  return { scannedSessions: 1, ingestedSessions: 1, appendedTurns: turns.length };
}

function emptyReport(): IngestReport {
  return { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };
}
