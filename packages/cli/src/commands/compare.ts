import {
  buildCompareTable,
  DEFAULT_MIN_SAMPLE,
  hasMinimumFidelity,
  loadPricing,
  summarizeFidelity,
  type FidelitySummary,
} from '@relayburn/analyze';
import { type EnrichedTurn, type Query } from '@relayburn/ledger';
import type { FidelityClass } from '@relayburn/reader';
import {
  compare as sdkCompare,
  type CompareCellResult,
  type CompareExcludedBreakdown,
  type CompareResult,
} from '@relayburn/sdk';

import { ingestAll } from '@relayburn/ingest';
import { formatInt, formatUsd, parseSinceArg } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { filterTurnsByProvider, parseProviderFilter } from '../provider.js';
import { withProgress } from '../progress.js';

const COMPARE_HELP = `burn compare — per-(model, activity) comparison table

Usage:
  burn compare <model_a,model_b[,...]> [--provider a,b] [--since 7d]
               [--project <path>] [--session <id>] [--workflow <id>]
               [--agent <id>] [--min-sample <n>] [--fidelity <class>]
               [--include-partial] [--json|--csv] [--no-archive]

Models:
  Required comma-separated positional argument naming at least two models
  to compare (e.g. claude-sonnet-4-6,claude-haiku-4-5). Use
  \`burn summary --by-provider\` (or \`burn summary --by-tool\`) to discover
  which models have data in your ledger.

Flags:
  --provider    comma-separated list of effective providers to include
                (e.g. synthetic, anthropic, openai); resolved via the same
                pricing-layer classifier summary/hotspots use
  --since       relative (e.g. 24h, 7d, 4w) or ISO timestamp; default: all time
  --project     filter by project path or git-canonical projectKey
  --session     filter by sessionId
  --workflow    filter by stamped workflowId
  --agent       filter by stamped agentId
  --min-sample  insufficient-sample threshold; cells below this get flagged
                in the coverage-notes block (default: 5)
  --fidelity    minimum fidelity class to include in the aggregate
                (full | usage-only | aggregate-only | cost-only | partial).
                Default: usage-only — drops aggregate-only / cost-only / partial
                turns so a session with mixed fidelity isn't silently averaged
                with full-fidelity turns from the same model. Records emitted
                before TurnRecord.fidelity existed always pass.
  --include-partial
                shorthand for --fidelity partial; includes every turn.
  --json        emit a stable JSON object (analyzedTurns, models, categories,
                totals, cells[], fidelity{ minimum, excluded, summary })
  --csv         emit a CSV with one row per (model, category) pair
  --no-archive  bypass the SQLite archive and stream the ledger directly
                (legacy path; honored when env RELAYBURN_ARCHIVE=0)
  --help, -h    show this message

Cell metrics:
  Turns       count of turns in the (model, activity) bucket
  Cost/turn   total cost / turn count, using the vendored pricing snapshot
  1-shot      oneShotTurns / editTurns; — for categories without edits
  JSON also exposes cacheHitRate and medianRetries per cell.

Missing-data cells render "—", never $0.00 or 0%.

Examples:
  burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 30d
  burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 7d
  burn compare claude-opus-4-7,claude-sonnet-4-6 --workflow wf-refactor --json
  burn compare claude-sonnet-4-6,claude-haiku-4-5 --fidelity full
  burn compare claude-sonnet-4-6,claude-haiku-4-5 --include-partial
`;

const NEEDS_MODELS_MSG =
  'burn compare: needs at least 2 models. Run `burn summary --by-provider` (or `burn summary --by-tool`) to see which models have data.\n';

const FIDELITY_CHOICES: ReadonlyArray<FidelityClass> = [
  'full',
  'usage-only',
  'aggregate-only',
  'cost-only',
  'partial',
];

export interface CompareDeps {
  ingestAll?: () => Promise<unknown>;
  queryAll?: (q: Query) => Promise<EnrichedTurn[]>;
  loadPricing?: typeof loadPricing;
}

export async function runCompare(
  args: ParsedArgs,
  deps: CompareDeps = {},
): Promise<number> {
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

  // `--models` was the old optional flag; the model list is now a required
  // comma-separated positional. Reject the flag explicitly so users on the old
  // shape land on a clear error pointing them at the new form, not a silent
  // "needs at least 2 models" message.
  if ('models' in args.flags) {
    process.stderr.write(
      `burn: --models was removed; pass models as a positional argument (e.g. \`burn compare claude-sonnet-4-6,claude-haiku-4-5\`).\n`,
    );
    return 2;
  }

  const seen = new Set<string>();
  const models: string[] = [];
  if (typeof first === 'string') {
    for (const raw of first.split(',')) {
      const m = raw.trim();
      if (!m) continue;
      if (seen.has(m)) continue;
      seen.add(m);
      models.push(m);
    }
  }
  if (models.length < 2) {
    process.stderr.write(NEEDS_MODELS_MSG);
    return 2;
  }

  const since = typeof args.flags['since'] === 'string' ? parseSinceArg(args.flags['since']) : undefined;
  const project = typeof args.flags['project'] === 'string' ? args.flags['project'] : undefined;
  const session = typeof args.flags['session'] === 'string' ? args.flags['session'] : undefined;
  const workflow = typeof args.flags['workflow'] === 'string' ? args.flags['workflow'] : undefined;
  const agent = typeof args.flags['agent'] === 'string' ? args.flags['agent'] : undefined;

  const providerFilter = parseProviderFilter(args.flags['provider']);
  if (providerFilter instanceof Error) {
    process.stderr.write(providerFilter.message);
    return 2;
  }
  const minSample = typeof args.flags['min-sample'] === 'string'
    ? Number(args.flags['min-sample'])
    : DEFAULT_MIN_SAMPLE;
  if (!Number.isFinite(minSample) || minSample < 1) {
    process.stderr.write(`burn: invalid --min-sample: ${args.flags['min-sample']}\n`);
    return 2;
  }

  // Resolve --fidelity / --include-partial. --include-partial is sugar for
  // --fidelity partial; passing both is fine if they agree, error otherwise.
  const includePartial = args.flags['include-partial'] === true;
  const fidelityFlag = args.flags['fidelity'];
  let minFidelity: FidelityClass = 'usage-only';
  if (typeof fidelityFlag === 'string') {
    if (!isFidelityClass(fidelityFlag)) {
      process.stderr.write(
        `burn: invalid --fidelity: ${fidelityFlag} (expected one of ${FIDELITY_CHOICES.join(', ')})\n`,
      );
      return 2;
    }
    minFidelity = fidelityFlag;
  }
  if (includePartial) {
    if (typeof fidelityFlag === 'string' && fidelityFlag !== 'partial') {
      process.stderr.write(
        `burn: --include-partial conflicts with --fidelity ${fidelityFlag}\n`,
      );
      return 2;
    }
    minFidelity = 'partial';
  }

  const wantJson = args.flags['json'] === true;
  const wantCsv = args.flags['csv'] === true;
  if (wantJson && wantCsv) {
    process.stderr.write(
      `burn: --json and --csv are mutually exclusive; pick one.\n`,
    );
    return 2;
  }

  // Test path (deps.queryAll injected). Bypass `sdk.compare` so tests stay
  // hermetic against the user's real `~/.relayburn/archive.sqlite`. Mirrors
  // the production pipeline against in-memory turns: provider filter, fidelity
  // gate, buildCompareTable, then the same `CompareResult` shape `sdkCompare`
  // returns.
  let result: CompareResult;
  if (deps.queryAll) {
    const ingest = deps.ingestAll ?? ingestAll;
    await withProgress('ingesting latest sessions', async (task) => {
      await ingest();
      task.succeed('ingested latest sessions');
    });
    const loadPricingFn = deps.loadPricing ?? loadPricing;
    const pricing = await withProgress('loading pricing snapshot', async (task) => {
      const loaded = await loadPricingFn();
      task.succeed('loaded pricing snapshot');
      return loaded;
    });
    const q: Query = {};
    if (since !== undefined) q.since = since;
    if (project !== undefined) q.project = project;
    if (session !== undefined) q.sessionId = session;
    if (workflow !== undefined || agent !== undefined) {
      q.enrichment = {};
      if (workflow !== undefined) q.enrichment.workflowId = workflow;
      if (agent !== undefined) q.enrichment.agentId = agent;
    }
    const queriedTurns = await withProgress('reading ledger turns', async (task) => {
      const turns = await deps.queryAll!(q);
      task.succeed(`read ${formatInt(turns.length)} turn${turns.length === 1 ? '' : 's'}`);
      return turns;
    });
    const turns = filterTurnsByProvider(queriedTurns, providerFilter);
    const summary = summarizeFidelity(turns);
    const filteredTurns = minFidelity === 'partial'
      ? turns
      : turns.filter((t) => hasMinimumFidelity(t.fidelity, minFidelity));
    const table = await withProgress('building compare table', async (task) => {
      const built = buildCompareTable(filteredTurns, { pricing, minSample, models });
      task.succeed(
        `built compare table from ${formatInt(filteredTurns.length)} turn` +
          `${filteredTurns.length === 1 ? '' : 's'}`,
      );
      return built;
    });
    result = shapeCompareResult(table, filteredTurns.length, minFidelity, summary);
  } else {
    await withProgress('ingesting latest sessions', (task) =>
      ingestAll({
        onProgress: (message) => task.update(`ingest: ${message}`),
        onWarn: (body) => task.warn(body),
      }),
    );
    // Honor --no-archive / RELAYBURN_ARCHIVE=0 by forcing the SDK off the
    // archive path. The SDK falls back to the ledger walk transparently when
    // the archive read fails, so the easiest way to express "ledger only" is
    // to set a non-default fidelity / provider — but those would change the
    // semantic gate. Instead, mirror the env contract here: the SDK reads
    // RELAYBURN_ARCHIVE itself via the underlying analyze layer where
    // applicable, and the CLI flag forwards by setting the env for the call.
    const restoreArchiveEnv = applyArchiveOverride(args);
    try {
      result = await withProgress('querying compare slice', async (task) => {
        const sdkOpts: Parameters<typeof sdkCompare>[0] = {
          models,
          minSample,
          minFidelity,
        };
        if (since !== undefined) sdkOpts.since = since;
        if (project !== undefined) sdkOpts.project = project;
        if (session !== undefined) sdkOpts.session = session;
        if (workflow !== undefined) sdkOpts.workflow = workflow;
        if (agent !== undefined) sdkOpts.agent = agent;
        if (providerFilter) sdkOpts.provider = [...providerFilter];
        sdkOpts.onLog = (msg: string) => task.update(msg);
        const r = await sdkCompare(sdkOpts);
        task.succeed(
          `built compare table from ${formatInt(r.analyzedTurns)} turn` +
            `${r.analyzedTurns === 1 ? '' : 's'}`,
        );
        return r;
      });
    } finally {
      restoreArchiveEnv();
    }
  }

  if (wantJson) {
    process.stdout.write(JSON.stringify(result, null, 2) + '\n');
    return 0;
  }
  if (wantCsv) {
    process.stdout.write(renderCsv(result));
    return 0;
  }

  process.stdout.write(renderTty(result));
  return 0;
}

function applyArchiveOverride(args: ParsedArgs): () => void {
  if (args.flags['no-archive'] !== true) return () => {};
  const prev = process.env['RELAYBURN_ARCHIVE'];
  process.env['RELAYBURN_ARCHIVE'] = '0';
  return () => {
    if (prev === undefined) delete process.env['RELAYBURN_ARCHIVE'];
    else process.env['RELAYBURN_ARCHIVE'] = prev;
  };
}

function isFidelityClass(s: string): s is FidelityClass {
  return (FIDELITY_CHOICES as ReadonlyArray<string>).includes(s);
}

function shapeCompareResult(
  table: ReturnType<typeof buildCompareTable>,
  analyzedTurns: number,
  minimum: FidelityClass,
  summary: FidelitySummary,
): CompareResult {
  const excluded = computeExcluded(summary, minimum);
  const cells: CompareCellResult[] = [];
  for (const m of table.models) {
    for (const cat of table.categories) {
      const c = table.cells[m]![cat]!;
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
    minSample: table.minSample,
    models: table.models,
    categories: table.categories,
    totals: table.totals,
    cells,
    fidelity: { minimum, excluded, summary: summary as unknown as CompareResult['fidelity']['summary'] },
  };
}

function computeExcluded(
  summary: FidelitySummary,
  minimum: FidelityClass,
): CompareExcludedBreakdown {
  const out: CompareExcludedBreakdown = {
    total: 0,
    aggregateOnly: 0,
    costOnly: 0,
    partial: 0,
    usageOnly: 0,
  };
  if (minimum === 'partial') return out;
  const order: ReadonlyArray<FidelityClass> = [
    'cost-only',
    'aggregate-only',
    'partial',
    'usage-only',
    'full',
  ];
  const need = order.indexOf(minimum);
  for (const cls of order) {
    if (order.indexOf(cls) >= need) continue;
    const n = summary.byClass[cls];
    if (n === 0) continue;
    out.total += n;
    if (cls === 'aggregate-only') out.aggregateOnly += n;
    else if (cls === 'cost-only') out.costOnly += n;
    else if (cls === 'partial') out.partial += n;
    else if (cls === 'usage-only') out.usageOnly += n;
  }
  return out;
}

function round(n: number, digits: number): number {
  return Number(n.toFixed(digits));
}

function renderCsv(r: CompareResult): string {
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
  // Iterate by (model × category) in the table order so column structure is
  // stable across runs; lookup into the flat cells keeps the body identical
  // to the legacy nested-map walk.
  const lookup = buildCellLookup(r);
  for (const m of r.models) {
    for (const cat of r.categories) {
      const c = lookup.get(m)!.get(cat)!;
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

const DASH = '—';

function formatPct(p: number): string {
  return `${Math.round(p * 100)}%`;
}

function cellFields(c: CompareCellResult): [string, string, string] {
  if (c.noData) return [DASH, DASH, DASH];
  const turns = formatInt(c.turns);
  const cost = c.costPerTurn !== null ? formatUsd(c.costPerTurn) : DASH;
  const oneShot = c.oneShotRate !== null ? formatPct(c.oneShotRate) : DASH;
  return [turns, cost, oneShot];
}

function renderTty(r: CompareResult): string {
  const lines: string[] = [];
  lines.push('');
  lines.push(`turns analyzed: ${formatInt(r.analyzedTurns)}`);
  if (r.fidelity.excluded.total > 0) {
    lines.push(formatExcludedNote(r.fidelity));
  }
  lines.push('');

  if (r.models.length === 0 || r.categories.length === 0) {
    lines.push('no data to compare (need turns spanning ≥1 model and ≥1 activity).');
    return lines.join('\n') + '\n';
  }

  const lookup = buildCellLookup(r);

  const subHeader: string[] = ['Activity'];
  for (let i = 0; i < r.models.length; i++) subHeader.push('Turns', 'Cost/turn', '1-shot');

  const dataRows: string[][] = [];
  for (const cat of r.categories) {
    const row: string[] = [cat];
    for (const m of r.models) {
      const [a, b, c] = cellFields(lookup.get(m)!.get(cat)!);
      row.push(a, b, c);
    }
    dataRows.push(row);
  }

  const widths = new Array(subHeader.length).fill(0);
  for (const row of [subHeader, ...dataRows]) {
    row.forEach((cell, i) => {
      widths[i] = Math.max(widths[i], cell.length);
    });
  }

  const SEP = '  ';

  for (let mi = 0; mi < r.models.length; mi++) {
    const start = 1 + mi * 3;
    const groupWidth = widths[start]! + SEP.length + widths[start + 1]! + SEP.length + widths[start + 2]!;
    const name = displayModelName(r.models[mi]!);
    if (name.length > groupWidth) widths[start + 2] += name.length - groupWidth;
  }

  const groupLine: string[] = [' '.repeat(widths[0]!)];
  for (let mi = 0; mi < r.models.length; mi++) {
    const start = 1 + mi * 3;
    const groupWidth = widths[start]! + SEP.length + widths[start + 1]! + SEP.length + widths[start + 2]!;
    const name = displayModelName(r.models[mi]!);
    groupLine.push(name.padEnd(groupWidth));
  }
  lines.push(groupLine.join(SEP).trimEnd());
  lines.push(renderRow(subHeader, widths, SEP));
  for (const row of dataRows) lines.push(renderRow(row, widths, SEP));

  const NOTE_LIMIT = 8;
  const notes: string[] = [];
  for (const cat of r.categories) {
    const anyHasData = r.models.some((m) => !lookup.get(m)!.get(cat)!.noData);
    if (!anyHasData) continue;
    for (const m of r.models) {
      const c = lookup.get(m)!.get(cat)!;
      if (c.noData) {
        notes.push(`no ${displayModelName(m)} data in '${cat}' — no comparison available.`);
      } else if (c.insufficientSample) {
        notes.push(
          `low ${displayModelName(m)} sample in '${cat}' (${c.turns} turns < ${r.minSample}) — treat as indicative.`,
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

  // Per-model totals. A model with zero turns renders cost as the dash
  // sentinel — `$0.00` would falsely claim the model ran for free.
  lines.push('');
  for (const m of r.models) {
    const tot = r.totals[m] ?? { turns: 0, totalCost: 0 };
    const totalCost = tot.turns > 0 ? formatUsd(tot.totalCost) : DASH;
    lines.push(`${displayModelName(m)}: ${formatInt(tot.turns)} turns, ${totalCost} total`);
  }
  lines.push('');
  return lines.join('\n');
}

function buildCellLookup(r: CompareResult): Map<string, Map<string, CompareCellResult>> {
  const out = new Map<string, Map<string, CompareCellResult>>();
  for (const cell of r.cells) {
    let byCat = out.get(cell.model);
    if (!byCat) {
      byCat = new Map();
      out.set(cell.model, byCat);
    }
    byCat.set(cell.category, cell);
  }
  return out;
}

function formatExcludedNote(f: CompareResult['fidelity']): string {
  const parts: string[] = [];
  if (f.excluded.aggregateOnly > 0) parts.push(`${f.excluded.aggregateOnly} aggregate-only`);
  if (f.excluded.costOnly > 0) parts.push(`${f.excluded.costOnly} cost-only`);
  if (f.excluded.partial > 0) parts.push(`${f.excluded.partial} partial`);
  if (f.excluded.usageOnly > 0) parts.push(`${f.excluded.usageOnly} usage-only`);
  const breakdown = parts.length > 0 ? ` (${parts.join(', ')})` : '';
  const noun = f.excluded.total === 1 ? 'turn' : 'turns';
  return `excluded ${formatInt(f.excluded.total)} ${noun} below ${f.minimum} fidelity${breakdown}`;
}

function renderRow(row: string[], widths: number[], sep: string): string {
  return row.map((cell, i) => cell.padEnd(widths[i]!)).join(sep).trimEnd();
}

function displayModelName(m: string): string {
  // Strip provider prefix (e.g. "anthropic/claude-sonnet-4-6" → "claude-sonnet-4-6")
  // for display brevity. Aggregation in buildCompareTable always uses the
  // raw model name, so two providers shipping a model with the same suffix
  // would still aggregate separately and only collide visually in the TTY
  // header. Use --json for the unambiguous identifier.
  const i = m.indexOf('/');
  return i >= 0 ? m.slice(i + 1) : m;
}
