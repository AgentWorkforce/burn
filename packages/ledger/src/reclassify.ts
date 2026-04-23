import { readFile, rename, stat, writeFile } from 'node:fs/promises';

import { classifyActivity } from '@relayburn/reader';
import type { ContentRecord, TurnRecord } from '@relayburn/reader';

import { readContent } from './content.js';
import { withLock } from './lock.js';
import { ledgerPath } from './paths.js';
import { isTurnLine, isStampLine, type TurnLine } from './schema.js';

export interface ReclassifyOptions {
  // When true, reclassify every turn even if it already has an activity set.
  // Default reclassifies only turns whose activity is undefined — safe to run
  // repeatedly without overwriting Claude turns that were classified using
  // signal we can't fully recover here (e.g. pruned content).
  force?: boolean;
}

export interface ReclassifyReport {
  // Total turn lines seen in the ledger.
  scanned: number;
  // Turn lines the classifier was actually re-run on.
  processed: number;
  // Of the processed turns, how many ended up with a different activity label.
  changed: number;
  // Turn lines the classifier was NOT re-run on (default mode only: turns
  // that already had an activity set).
  skipped: number;
  // Breakdown of the `changed` count by the turn's new activity.
  changedByCategory: Record<string, number>;
}

interface RawLine {
  raw: string;
  parsed: TurnLine | null;
  modified: boolean;
}

export async function reclassifyLedger(opts: ReclassifyOptions = {}): Promise<ReclassifyReport> {
  const force = opts.force === true;
  const report: ReclassifyReport = {
    scanned: 0,
    processed: 0,
    changed: 0,
    skipped: 0,
    changedByCategory: {},
  };
  const filePath = ledgerPath();

  const exists = await stat(filePath)
    .then((s) => s.isFile())
    .catch(() => false);
  if (!exists) return report;

  return withLock('ledger', async () => {
    const raw = await readFile(filePath, 'utf8');
    // Preserve trailing newline so the rewritten file keeps the same layout.
    const hasTrailingNewline = raw.endsWith('\n');
    const bodies = hasTrailingNewline ? raw.slice(0, -1).split('\n') : raw.split('\n');

    const lines: RawLine[] = bodies.map((rawLine) => {
      const trimmed = rawLine.trim();
      if (!trimmed) return { raw: rawLine, parsed: null, modified: false };
      try {
        const parsed = JSON.parse(trimmed);
        if (isTurnLine(parsed)) return { raw: rawLine, parsed, modified: false };
        if (isStampLine(parsed)) return { raw: rawLine, parsed: null, modified: false };
      } catch {
        // fall through
      }
      return { raw: rawLine, parsed: null, modified: false };
    });

    // Group turns by sessionId for per-session content lookup.
    const bySession = new Map<string, RawLine[]>();
    for (const line of lines) {
      if (!line.parsed) continue;
      const sid = line.parsed.record.sessionId;
      let list = bySession.get(sid);
      if (!list) {
        list = [];
        bySession.set(sid, list);
      }
      list.push(line);
    }

    for (const [sessionId, turnLines] of bySession) {
      turnLines.sort((a, b) => a.parsed!.record.ts.localeCompare(b.parsed!.record.ts));
      let contentRecs: ContentRecord[] = [];
      try {
        contentRecs = await readContent({ sessionId });
      } catch {
        contentRecs = [];
      }
      const { erroredIds, userTexts, assistantTextByMsg } = indexContent(contentRecs);

      let prevTs = '';
      for (const line of turnLines) {
        const turnLine = line.parsed!;
        const rec = turnLine.record;
        report.scanned++;
        if (!force && rec.activity !== undefined) {
          report.skipped++;
          prevTs = rec.ts;
          continue;
        }

        const text = collectTextForTurn(userTexts, assistantTextByMsg, prevTs, rec);
        const hasFailedTool = rec.toolCalls.some((tc) => erroredIds.has(tc.id));
        const result = classifyActivity({
          toolCalls: rec.toolCalls,
          text,
          hasFailedTool,
          reasoningTokens: rec.usage.reasoning,
        });
        report.processed++;
        const previous = rec.activity;
        if (result.activity !== previous) {
          report.changed++;
          report.changedByCategory[result.activity] =
            (report.changedByCategory[result.activity] ?? 0) + 1;
        }
        rec.activity = result.activity;
        rec.retries = result.retries;
        rec.hasEdits = result.hasEdits;
        line.modified = true;
        prevTs = rec.ts;
      }
    }

    if (lines.every((l) => !l.modified)) {
      return report;
    }

    // Rewrite ledger atomically.
    const out = lines
      .map((l) => (l.modified && l.parsed ? JSON.stringify(l.parsed) : l.raw))
      .join('\n');
    const body = hasTrailingNewline ? out + '\n' : out;
    const tmp = filePath + '.tmp';
    await writeFile(tmp, body, 'utf8');
    await rename(tmp, filePath);
    return report;
  });
}

interface ContentIndex {
  erroredIds: Set<string>;
  userTexts: Array<{ ts: string; text: string }>;
  assistantTextByMsg: Map<string, string[]>;
}

function indexContent(records: ContentRecord[]): ContentIndex {
  const erroredIds = new Set<string>();
  const userTexts: Array<{ ts: string; text: string }> = [];
  const assistantTextByMsg = new Map<string, string[]>();
  for (const cr of records) {
    if (cr.kind === 'tool_result') {
      const tr = cr.toolResult;
      if (tr?.isError === true && typeof tr.toolUseId === 'string') {
        erroredIds.add(tr.toolUseId);
      }
      continue;
    }
    if (cr.kind !== 'text') continue;
    const text = cr.text;
    if (typeof text !== 'string' || text.length === 0) continue;
    if (cr.role === 'user') {
      userTexts.push({ ts: cr.ts, text });
    } else if (cr.role === 'assistant') {
      let list = assistantTextByMsg.get(cr.messageId);
      if (!list) {
        list = [];
        assistantTextByMsg.set(cr.messageId, list);
      }
      list.push(text);
    }
  }
  userTexts.sort((a, b) => a.ts.localeCompare(b.ts));
  return { erroredIds, userTexts, assistantTextByMsg };
}

function collectTextForTurn(
  userTexts: ContentIndex['userTexts'],
  assistantTextByMsg: ContentIndex['assistantTextByMsg'],
  prevTs: string,
  rec: TurnRecord,
): string {
  const chunks: string[] = [];
  for (const ut of userTexts) {
    if (ut.ts <= prevTs) continue;
    if (ut.ts > rec.ts) break;
    chunks.push(ut.text);
  }
  const assistant = assistantTextByMsg.get(rec.messageId);
  if (assistant && assistant.length > 0) chunks.push(assistant.join('\n'));
  return chunks.join('\n');
}
