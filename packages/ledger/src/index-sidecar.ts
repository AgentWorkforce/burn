import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { appendFile, mkdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import * as path from 'node:path';

import type {
  CompactionEvent,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { withLock } from './lock.js';
import {
  ledgerContentIndexPath,
  ledgerHome,
  ledgerIndexPath,
  ledgerPath,
} from './paths.js';
import {
  isCompactionLine,
  isSessionRelationshipLine,
  isToolResultEventLine,
  isTurnLine,
  isUserTurnLine,
} from './schema.js';

export const CONTENT_WINDOW = 10_000;

export function turnIdHash(t: {
  source: string;
  sessionId: string;
  messageId: string;
}): string {
  return createHash('sha256')
    .update(`${t.source}|${t.sessionId}|${t.messageId}`)
    .digest('hex')
    .slice(0, 16);
}

export function compactionIdHash(e: CompactionEvent): string {
  return createHash('sha256')
    .update(`${e.source}|${e.sessionId}|${e.ts}`)
    .digest('hex')
    .slice(0, 16);
}

// Stable id for a SessionRelationshipRecord. Roots dedupe per session, so
// the type is enough for them; subagent / fork / continuation rows include
// agentId (the per-invocation stable id) when available, falling back to
// parentToolUseId for older records that didn't resolve agentId. Hashes
// share the ids namespace with turnIdHash / compactionIdHash — different
// inputs make a collision astronomically unlikely.
export function relationshipIdHash(r: SessionRelationshipRecord): string {
  const key = [
    r.source,
    r.sessionId,
    r.relationshipType,
    r.relatedSessionId ?? '',
    r.agentId ?? '',
    r.parentToolUseId ?? '',
  ].join('|');
  return createHash('sha256').update(key).digest('hex').slice(0, 16);
}

// Stable id for a ToolResultEventRecord. (sessionId, toolUseId, eventIndex)
// is the unique tuple — eventIndex is monotonic per parser pass and per
// session so two passes that re-read the same line produce the same id.
export function toolResultEventIdHash(r: ToolResultEventRecord): string {
  const key = [r.source, r.sessionId, r.toolUseId, r.eventIndex].join('|');
  return createHash('sha256').update(key).digest('hex').slice(0, 16);
}

// Stable id for a UserTurnRecord. (source, sessionId, userUuid) uniquely
// identifies a user line within the source — the parser preserves the source
// log's per-line uuid so two ingest passes re-reading the same bytes produce
// the same id. Shares the ledger-id namespace with turns / compactions /
// relationships / tool-result events; the hash inputs differ enough that
// collisions are not a practical concern.
export function userTurnIdHash(r: UserTurnRecord): string {
  return createHash('sha256')
    .update(`${r.source}|${r.sessionId}|${r.userUuid}`)
    .digest('hex')
    .slice(0, 16);
}

export function turnContentFingerprint(t: TurnRecord): string {
  const firstToolArgsPrefix =
    t.toolCalls.length > 0 && typeof t.toolCalls[0]?.argsHash === 'string'
      ? t.toolCalls[0]!.argsHash.slice(0, 4)
      : '';
  const composite = [
    t.ts,
    t.model,
    t.usage.input + t.usage.output,
    t.usage.cacheRead,
    t.usage.cacheCreate5m + t.usage.cacheCreate1h,
    firstToolArgsPrefix,
  ].join('|');
  return createHash('sha256').update(composite).digest('hex').slice(0, 16);
}

// Keyed on ledgerHome() so a `RELAYBURN_HOME` change (tests, CLI invocations
// that re-parent the ledger dir) invalidates the cache automatically — the
// hashes from the prior home would otherwise mask records under the new one.
let cache:
  | { home: string; ids: Set<string>; content: Set<string>; contentOrder: string[] }
  | undefined;

export async function loadIndex(): Promise<{ ids: Set<string>; content: Set<string> }> {
  const home = ledgerHome();
  if (cache && cache.home === home) return { ids: cache.ids, content: cache.content };
  const ids = await loadHashFile(ledgerIndexPath());
  const contentLines = await loadHashFileAsArray(ledgerContentIndexPath());
  const tail = contentLines.slice(Math.max(0, contentLines.length - CONTENT_WINDOW));
  const content = new Set(tail);
  cache = { home, ids, content, contentOrder: [...tail] };
  return { ids, content };
}

// Drop the in-memory dedup cache. Callers that wipe the on-disk index
// (`burn state reset`, etc.) MUST call this after deletion so the next
// loadIndex() re-reads from the empty files instead of returning hashes
// loaded before the wipe — otherwise post-reset writes get silently
// deduped against records that no longer exist.
export function invalidateIndexCache(): void {
  cache = undefined;
}

// Test alias kept for back-compat with existing call sites.
export const __resetIndexCacheForTesting = invalidateIndexCache;

async function loadHashFile(p: string): Promise<Set<string>> {
  const lines = await loadHashFileAsArray(p);
  return new Set(lines);
}

async function loadHashFileAsArray(p: string): Promise<string[]> {
  try {
    const raw = await readFile(p, 'utf8');
    return raw
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0);
  } catch {
    return [];
  }
}

export async function appendHashes(idHashes: string[], contentHashes: string[]): Promise<void> {
  if (idHashes.length === 0 && contentHashes.length === 0) return;
  await mkdir(path.dirname(ledgerIndexPath()), { recursive: true });
  await withLock('ledger-index', async () => {
    if (idHashes.length > 0) {
      await appendFile(ledgerIndexPath(), idHashes.join('\n') + '\n', 'utf8');
    }
    if (contentHashes.length > 0) {
      // Maintain rolling window on disk. loadIndex() is home-aware, so this
      // also reloads after a RELAYBURN_HOME swap.
      await loadIndex();
      const order = cache!.contentOrder;
      for (const h of contentHashes) order.push(h);
      if (order.length > CONTENT_WINDOW) {
        const trimmed = order.slice(order.length - CONTENT_WINDOW);
        cache!.contentOrder = trimmed;
        const tmp = `${ledgerContentIndexPath()}.tmp`;
        await writeFile(tmp, trimmed.join('\n') + '\n', 'utf8');
        await rename(tmp, ledgerContentIndexPath());
      } else {
        await appendFile(ledgerContentIndexPath(), contentHashes.join('\n') + '\n', 'utf8');
      }
    }
  });
}

export async function rebuildIndex(): Promise<{ ids: number; content: number }> {
  const ledger = ledgerPath();
  const ids = new Set<string>();
  const contentOrder: string[] = [];
  const contentSeen = new Set<string>();

  const exists = await stat(ledger)
    .then((s) => s.isFile())
    .catch(() => false);
  if (exists) {
    const rl = createInterface({
      input: createReadStream(ledger, { encoding: 'utf8' }),
      crlfDelay: Infinity,
    });
    try {
      for await (const line of rl) {
        const t = line.trim();
        if (!t) continue;
        let parsed: unknown;
        try {
          parsed = JSON.parse(t);
        } catch {
          continue;
        }
        if (isTurnLine(parsed)) {
          const r = parsed.record;
          ids.add(turnIdHash(r));
          const cf = turnContentFingerprint(r);
          if (!contentSeen.has(cf)) {
            contentSeen.add(cf);
            contentOrder.push(cf);
          }
        } else if (isCompactionLine(parsed)) {
          ids.add(compactionIdHash(parsed.record));
        } else if (isSessionRelationshipLine(parsed)) {
          ids.add(relationshipIdHash(parsed.record));
        } else if (isToolResultEventLine(parsed)) {
          ids.add(toolResultEventIdHash(parsed.record));
        } else if (isUserTurnLine(parsed)) {
          ids.add(userTurnIdHash(parsed.record));
        }
      }
    } finally {
      rl.close();
    }
  }

  await mkdir(path.dirname(ledgerIndexPath()), { recursive: true });
  const idsBody = ids.size > 0 ? [...ids].join('\n') + '\n' : '';
  const contentTail = contentOrder.slice(
    Math.max(0, contentOrder.length - CONTENT_WINDOW),
  );
  const contentBody = contentTail.length > 0 ? contentTail.join('\n') + '\n' : '';

  await withLock('ledger-index', async () => {
    const idsTmp = `${ledgerIndexPath()}.tmp`;
    const contentTmp = `${ledgerContentIndexPath()}.tmp`;
    await writeFile(idsTmp, idsBody, 'utf8');
    await rename(idsTmp, ledgerIndexPath());
    await writeFile(contentTmp, contentBody, 'utf8');
    await rename(contentTmp, ledgerContentIndexPath());
  });

  cache = {
    home: ledgerHome(),
    ids: new Set(ids),
    content: new Set(contentTail),
    contentOrder: [...contentTail],
  };

  return { ids: ids.size, content: contentTail.length };
}
