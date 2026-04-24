import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { appendFile, mkdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import * as path from 'node:path';

import type { CompactionEvent, TurnRecord } from '@relayburn/reader';

import { withLock } from './lock.js';
import {
  ledgerContentIndexPath,
  ledgerIndexPath,
  ledgerPath,
} from './paths.js';
import { isCompactionLine, isTurnLine } from './schema.js';

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

let cache: { ids: Set<string>; content: Set<string>; contentOrder: string[] } | undefined;

export async function loadIndex(): Promise<{ ids: Set<string>; content: Set<string> }> {
  if (cache) return { ids: cache.ids, content: cache.content };
  const ids = await loadHashFile(ledgerIndexPath());
  const contentLines = await loadHashFileAsArray(ledgerContentIndexPath());
  const tail = contentLines.slice(Math.max(0, contentLines.length - CONTENT_WINDOW));
  const content = new Set(tail);
  cache = { ids, content, contentOrder: [...tail] };
  return { ids, content };
}

export function __resetIndexCacheForTesting(): void {
  cache = undefined;
}

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
      // Maintain rolling window on disk
      if (!cache) await loadIndex();
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
    ids: new Set(ids),
    content: new Set(contentTail),
    contentOrder: [...contentTail],
  };

  return { ids: ids.size, content: contentTail.length };
}
