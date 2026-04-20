import { appendFile, mkdir } from 'node:fs/promises';
import * as path from 'node:path';

import type { TurnRecord } from '@relayburn/reader';

import { ledgerPath } from './paths.js';
import type { Enrichment, LedgerLine, StampLine, StampSelector, TurnLine } from './schema.js';

async function ensureDir(filePath: string): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });
}

async function appendLines(lines: LedgerLine[]): Promise<void> {
  if (lines.length === 0) return;
  const filePath = ledgerPath();
  await ensureDir(filePath);
  const payload = lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
  await appendFile(filePath, payload, { encoding: 'utf8' });
}

export async function appendTurns(turns: TurnRecord[]): Promise<void> {
  const lines: TurnLine[] = turns.map((record) => ({ v: 1, kind: 'turn', record }));
  await appendLines(lines);
}

export async function stamp(
  selector: StampSelector,
  enrichment: Enrichment,
): Promise<void> {
  if (
    selector.sessionId === undefined &&
    selector.messageId === undefined &&
    selector.range === undefined
  ) {
    throw new Error('stamp requires at least one selector field (sessionId, messageId, or range)');
  }
  const line: StampLine = {
    v: 1,
    kind: 'stamp',
    ts: new Date().toISOString(),
    selector,
    enrichment,
  };
  await appendLines([line]);
}
