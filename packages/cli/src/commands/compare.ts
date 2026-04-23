import {
  buildCompareTable,
  DEFAULT_MIN_SAMPLE,
  loadPricing,
  type CompareCell,
  type CompareTable,
} from '@relayburn/analyze';
import { queryAll, type Query } from '@relayburn/ledger';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg } from '../format.js';
import type { ParsedArgs } from '../args.js';

const COMPARE_HELP = `burn compare — per-(model, activity) comparison table

Usage:
  burn compare [--models a,b] [--since 7d] [--project <path>] [--session <id>]
               [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]

Flags:
  --models      comma-separated list of model names to include (default: all)
  --since       relative (e.g. 24h, 7d, 4w) or ISO timestamp; default: all time
  --project     filter by project path or git-canonical projectKey
  --session     filter by sessionId
  --workflow    filter by stamped workflowId
  --agent       filter by stamped agentId
  --min-sample  insufficient-sample threshold; cells below this get flagged
                in the coverage-notes block (default: 5)
  --json        emit a stable JSON object (analyzedTurns, models, categories,
                totals, cells[])
  --csv         emit a CSV with one row per (model, category) pair
  --help, -h    show this message

Cell metrics:
  Turns       count of turns in the (model, activity) bucket
  Cost/turn   total cost / turn count, using the vendored pricing snapshot
  1-shot      oneShotTurns / editTurns; — for categories without edits
  JSON also exposes cacheHitRate and medianRetries per cell.

Missing-data cells render "—", never $0.00 or 0%.

Examples:
  burn compare --since 30d
  burn compare --models claude-sonnet-4-6,claude-haiku-4-5 --since 7d
  burn compare --workflow wf-refactor --json
`;

export async function runCompare(args: ParsedArgs): Promise<number> {
  const first = args.positional[0];
  if (
    args.flags['help'] === true ||
    first === 'help' ||
    first === '-h' ||
    first === '--help'
  ) {
    process.stdout.write(COMPARE_HELP);
    return 0;
  }
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  if (typeof args.flags['agent'] === 'string') q.enrichment = { ...(q.enrichment ?? {}), agentId: args.flags['agent'] };

  const modelsArg = typeof args.flags['models'] === 'string' ? args.flags['models'] : undefined;
  const models = modelsArg ? modelsArg.split(',').map((s) => s.trim()).filter(Boolean) : undefined;
  const minSample = typeof args.flags['min-sample'] === 'string'
    ? Number(args.flags['min-sample'])
    : DEFAULT_MIN_SAMPLE;
  if (!Number.isFinite(minSample) || minSample < 1) {
    process.stderr.write(`burn: invalid --min-sample: ${args.flags['min-sample']}\n`);
    return 2;
  }

  const wantJson = args.flags['json'] === true;
  const wantCsv = args.flags['csv'] === true;
  if (wantJson && wantCsv) {
    process.stderr.write(
      `burn: --json and --csv are mutually exclusive; pick one.\n`,
    );
    return 2;
  }

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);

  const opts: Parameters<typeof buildCompareTable>[1] = { pricing, minSample };
  if (models) opts.models = models;
  const table = buildCompareTable(turns, opts);

  if (wantJson) {
    process.stdout.write(JSON.stringify(toJson(table, turns.length), null, 2) + '\n');
    return 0;
  }
  if (wantCsv) {
    process.stdout.write(renderCsv(table));
    return 0;
  }

  process.stdout.write(renderTty(table, turns.length));
  return 0;
}

function toJson(t: CompareTable, analyzedTurns: number): object {
  const cells: Array<Record<string, unknown>> = [];
  for (const m of t.models) {
    for (const cat of t.categories) {
      const c = t.cells[m]![cat]!;
      cells.push({
        model: m,
        category: cat,
        turns: c.turns,
        editTurns: c.editTurns,
        oneShotTurns: c.oneShotTurns,
        pricedTurns: c.pricedTurns,
        totalCost: round(c.totalCost, 6),
        costPerTurn: c.costPerTurn !== null ? round(c.costPerTurn, 6) : null,
        oneShotRate: c.oneShotRate !== null ? round(c.oneShotRate, 4) : null,
        cacheHitRate: c.cacheHitRate !== null ? round(c.cacheHitRate, 4) : null,
        medianRetries: c.medianRetries,
        noData: c.noData,
        insufficientSample: c.insufficientSample,
      });
    }
  }
  return {
    analyzedTurns,
    minSample: t.minSample,
    models: t.models,
    categories: t.categories,
    totals: t.totals,
    cells,
  };
}

function renderCsv(t: CompareTable): string {
  const header = [
    'model',
    'category',
    'turns',
    'editTurns',
    'oneShotTurns',
    'pricedTurns',
    'totalCost',
    'costPerTurn',
    'oneShotRate',
    'cacheHitRate',
    'medianRetries',
    'noData',
    'insufficientSample',
  ];
  const rows: string[] = [header.join(',')];
  for (const m of t.models) {
    for (const cat of t.categories) {
      const c = t.cells[m]![cat]!;
      rows.push(
        [
          csvCell(m),
          csvCell(cat),
          String(c.turns),
          String(c.editTurns),
          String(c.oneShotTurns),
          String(c.pricedTurns),
          numCsv(c.totalCost, 6),
          c.costPerTurn !== null ? numCsv(c.costPerTurn, 6) : '',
          c.oneShotRate !== null ? numCsv(c.oneShotRate, 4) : '',
          c.cacheHitRate !== null ? numCsv(c.cacheHitRate, 4) : '',
          c.medianRetries !== null ? String(c.medianRetries) : '',
          c.noData ? 'true' : 'false',
          c.insufficientSample ? 'true' : 'false',
        ].join(','),
      );
    }
  }
  return rows.join('\n') + '\n';
}

function csvCell(s: string): string {
  if (s.includes(',') || s.includes('"') || s.includes('\n')) {
    return `"${s.replace(/"/g, '""')}"`;
  }
  return s;
}

function numCsv(n: number, digits: number): string {
  return Number(n.toFixed(digits)).toString();
}

function round(n: number, digits: number): number {
  return Number(n.toFixed(digits));
}

const DASH = '—';

function formatPct(p: number): string {
  return `${Math.round(p * 100)}%`;
}

function cellFields(c: CompareCell): [string, string, string] {
  if (c.noData) return [DASH, DASH, DASH];
  const turns = formatInt(c.turns);
  // Cost dashes when no turns in the cell were priced (unknown/unpriced model)
  // — distinct from $0.00 which would be a meaningful "free" claim.
  const cost = c.costPerTurn !== null ? formatUsd(c.costPerTurn) : DASH;
  // One-shot rate dashes for categories that don't produce edits
  // (exploration, brainstorming, planning, delegation, testing, git, deps,
  // format, build-deploy, verification, review, reasoning, conversation).
  const oneShot = c.oneShotRate !== null ? formatPct(c.oneShotRate) : DASH;
  return [turns, cost, oneShot];
}

function renderTty(t: CompareTable, analyzedTurns: number): string {
  const lines: string[] = [];
  lines.push('');
  lines.push(`turns analyzed: ${formatInt(analyzedTurns)}`);
  lines.push('');

  if (t.models.length === 0 || t.categories.length === 0) {
    lines.push('no data to compare (need turns spanning ≥1 model and ≥1 activity).');
    return lines.join('\n') + '\n';
  }

  // Build sub-header and data rows first to measure widths.
  const subHeader: string[] = ['Activity'];
  for (let i = 0; i < t.models.length; i++) subHeader.push('Turns', 'Cost/turn', '1-shot');

  const dataRows: string[][] = [];
  for (const cat of t.categories) {
    const row: string[] = [cat];
    for (const m of t.models) {
      const [a, b, c] = cellFields(t.cells[m]![cat]!);
      row.push(a, b, c);
    }
    dataRows.push(row);
  }

  // Compute column widths across sub-header + data rows.
  const widths = new Array(subHeader.length).fill(0);
  for (const row of [subHeader, ...dataRows]) {
    row.forEach((cell, i) => {
      widths[i] = Math.max(widths[i], cell.length);
    });
  }

  const SEP = '  ';

  // If a model display name is wider than its three sub-columns combined,
  // widen the last sub-column so the group header and sub-header stay aligned.
  for (let mi = 0; mi < t.models.length; mi++) {
    const start = 1 + mi * 3;
    const groupWidth = widths[start]! + SEP.length + widths[start + 1]! + SEP.length + widths[start + 2]!;
    const name = displayModelName(t.models[mi]!);
    if (name.length > groupWidth) widths[start + 2] += name.length - groupWidth;
  }

  const groupLine: string[] = [' '.repeat(widths[0]!)];
  for (let mi = 0; mi < t.models.length; mi++) {
    const start = 1 + mi * 3;
    const groupWidth = widths[start]! + SEP.length + widths[start + 1]! + SEP.length + widths[start + 2]!;
    const name = displayModelName(t.models[mi]!);
    groupLine.push(name.padEnd(groupWidth));
  }
  lines.push(groupLine.join(SEP).trimEnd());
  lines.push(renderRow(subHeader, widths, SEP));
  for (const row of dataRows) lines.push(renderRow(row, widths, SEP));

  // Coverage notes. Only surface a gap when at least one other model has
  // data in that category — otherwise the row is already uniformly empty and
  // the note is noise. Cap at NOTE_LIMIT; overflow gets a summary line.
  const NOTE_LIMIT = 8;
  const notes: string[] = [];
  for (const cat of t.categories) {
    const anyHasData = t.models.some((m) => !t.cells[m]![cat]!.noData);
    if (!anyHasData) continue;
    for (const m of t.models) {
      const c = t.cells[m]![cat]!;
      if (c.noData) {
        notes.push(`no ${displayModelName(m)} data in '${cat}' — no comparison available.`);
      } else if (c.insufficientSample) {
        notes.push(
          `low ${displayModelName(m)} sample in '${cat}' (${c.turns} turns < ${t.minSample}) — treat as indicative.`,
        );
      }
    }
  }
  if (notes.length > 0) {
    lines.push('');
    const shown = notes.slice(0, NOTE_LIMIT);
    for (const n of shown) lines.push(`  ${n}`);
    if (notes.length > NOTE_LIMIT) {
      lines.push(`  … and ${notes.length - NOTE_LIMIT} more coverage gaps.`);
    }
  }

  // Per-model totals
  lines.push('');
  for (const m of t.models) {
    const tot = t.totals[m] ?? { turns: 0, totalCost: 0 };
    lines.push(`${displayModelName(m)}: ${formatInt(tot.turns)} turns, ${formatUsd(tot.totalCost)} total`);
  }
  lines.push('');
  return lines.join('\n');
}

function renderRow(row: string[], widths: number[], sep: string): string {
  return row.map((cell, i) => cell.padEnd(widths[i]!)).join(sep).trimEnd();
}

function displayModelName(m: string): string {
  // Strip provider prefix (e.g. "anthropic/claude-sonnet-4-6" → "claude-sonnet-4-6")
  // for display brevity. Aggregation in buildCompareTable always uses the
  // raw model name, so two providers that ship a model with the same suffix
  // would still aggregate separately and only collide visually in the TTY
  // header. Use --json for the unambiguous identifier.
  const i = m.indexOf('/');
  return i >= 0 ? m.slice(i + 1) : m;
}
