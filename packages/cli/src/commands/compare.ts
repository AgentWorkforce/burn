import {
  buildCompareTable,
  compareFromArchive,
  DEFAULT_MIN_SAMPLE,
  hasMinimumFidelity,
  loadPricing,
  summarizeFidelity,
  type CompareCell,
  type CompareTable,
  type FidelitySummary,
} from '@relayburn/analyze';
import { buildArchive, queryAll, type EnrichedTurn, type Query } from '@relayburn/ledger';
import type { FidelityClass } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { filterTurnsByProvider, parseProviderFilter } from '../provider.js';

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

  // `--models` was the old optional flag; #159 made the model list a required
  // comma-separated positional. Reject the flag explicitly so users on the old
  // shape land on a clear error pointing them at the new form, not a silent
  // "needs at least 2 models" message.
  if ('models' in args.flags) {
    process.stderr.write(
      `burn: --models was removed; pass models as a positional argument (e.g. \`burn compare claude-sonnet-4-6,claude-haiku-4-5\`).\n`,
    );
    return 2;
  }

  // Required positional: a comma-separated list of >=2 models. Same
  // trim/dedupe rules the old --models flag used.
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

  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  if (typeof args.flags['agent'] === 'string') q.enrichment = { ...(q.enrichment ?? {}), agentId: args.flags['agent'] };
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

  // Resolve --fidelity / --include-partial. --include-partial is just sugar
  // for --fidelity partial; passing both is fine as long as they agree, and
  // we error otherwise so the user doesn't get a surprising effective level.
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

  const ingest = deps.ingestAll ?? ingestAll;
  const query = deps.queryAll ?? queryAll;
  const loadPricingFn = deps.loadPricing ?? loadPricing;

  await ingest();
  const pricing = await loadPricingFn();

  const opts: Parameters<typeof buildCompareTable>[1] = { pricing, minSample, models };

  // Archive path is the default (#88), but it does not yet apply
  // `attribution_fidelity` filtering at the SQL layer. Fall back to the
  // in-memory `queryAll` + `buildCompareTable` path whenever fidelity
  // filtering is in effect (i.e., minFidelity !== 'partial') so #95's
  // gate is correctly applied. `--include-partial` / `--fidelity partial`
  // disables filtering and reuses the archive's grouped SQL.
  //
  // `--provider` likewise bypasses the archive: provider is derived per
  // turn from (model, source) at query time, and the archive's grouped
  // SQL doesn't expose that classifier. The in-memory path applies the
  // filter via `filterTurnsByProvider` before `buildCompareTable`.
  //
  // Skip the archive when the caller injected `queryAll` (test mode):
  // `buildArchive` and `compareFromArchive` are not part of `CompareDeps`
  // and would hit the real `~/.relayburn/archive.sqlite`, breaking test
  // isolation. The non-archive branch already handles `--fidelity partial`
  // correctly (the filter becomes a no-op).
  const useArchive =
    !shouldBypassArchive(args) &&
    minFidelity === 'partial' &&
    !providerFilter &&
    !deps.queryAll;
  let table: CompareTable;
  let filteredTurns: EnrichedTurn[];
  let summary: FidelitySummary;
  if (useArchive) {
    // Materialize the ledger tail before reading. ingestAll() above only
    // writes to the JSONL ledger; the archive is a derived read model that
    // needs an explicit catch-up build to reflect the just-appended turns.
    await buildArchive();
    const result = await compareFromArchive(q, opts);
    table = result.table;
    // For fidelity-permissive mode the JSON summary reflects everything in
    // the queried slice; we still emit a zero-excluded breakdown so the
    // schema is stable.
    const turnsForSummary = await query(q);
    summary = summarizeFidelity(turnsForSummary);
    filteredTurns = turnsForSummary;
    void result.analyzedTurns;
  } else {
    // `--provider` narrows the queried slice up front so coverage / fidelity
    // counts reflect the provider-scoped input, not the cross-provider total
    // (which would over-count "excluded" turns relative to what's rendered).
    const turns = filterTurnsByProvider(await query(q), providerFilter);
    // Summarize fidelity over the *unfiltered* slice (modulo provider) so
    // coverage notes and the JSON `summary` reflect the input the user
    // actually queried, not what survived the fidelity gate. The summary
    // is what tells them why N turns were dropped.
    summary = summarizeFidelity(turns);
    // `--fidelity partial` (and its `--include-partial` shorthand) is the
    // "let everything through" escape hatch per #41. The FidelityClass
    // ordering used by `hasMinimumFidelity` puts `partial` strictly above
    // `aggregate-only` / `cost-only`, so the predicate would otherwise
    // still drop those two buckets. Bypass the gate entirely in that mode.
    filteredTurns = minFidelity === 'partial'
      ? turns
      : turns.filter((t) => hasMinimumFidelity(t.fidelity, minFidelity));
    table = buildCompareTable(filteredTurns, opts);
  }
  const excluded = computeExcluded(summary, minFidelity);

  if (wantJson) {
    process.stdout.write(
      JSON.stringify(
        toJson(table, filteredTurns.length, {
          minimum: minFidelity,
          excluded,
          summary,
        }),
        null,
        2,
      ) + '\n',
    );
    return 0;
  }
  if (wantCsv) {
    process.stdout.write(renderCsv(table));
    return 0;
  }

  process.stdout.write(
    renderTty(table, filteredTurns.length, { minimum: minFidelity, excluded }),
  );
  return 0;
}

function shouldBypassArchive(args: ParsedArgs): boolean {
  if (args.flags['no-archive'] === true) return true;
  const env = process.env['RELAYBURN_ARCHIVE'];
  if (env === '0' || env === 'false' || env === 'no') return true;
  return false;
}

function isFidelityClass(s: string): s is FidelityClass {
  return (FIDELITY_CHOICES as ReadonlyArray<string>).includes(s);
}

interface ExcludedBreakdown {
  total: number;
  aggregateOnly: number;
  costOnly: number;
  partial: number;
  usageOnly: number;
}

// Sum the byClass buckets that fall below the minimum fidelity. We never
// exclude `unknown` (records without a fidelity field — `hasMinimumFidelity`
// passes them for backward compat), so they don't get counted here.
//
// `--fidelity partial` is the "include everything" escape hatch (matched by
// the runtime), so it always reports zero excluded — even though the
// FidelityClass ordering puts `partial` above `aggregate-only` / `cost-only`.
function computeExcluded(
  summary: FidelitySummary,
  minimum: FidelityClass,
): ExcludedBreakdown {
  const out: ExcludedBreakdown = {
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

interface FidelityJsonBlock {
  minimum: FidelityClass;
  excluded: ExcludedBreakdown;
  summary: FidelitySummary;
}

function toJson(
  t: CompareTable,
  analyzedTurns: number,
  fidelity: FidelityJsonBlock,
): object {
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
    fidelity,
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

interface FidelityRenderInput {
  minimum: FidelityClass;
  excluded: ExcludedBreakdown;
}

function renderTty(
  t: CompareTable,
  analyzedTurns: number,
  fidelity: FidelityRenderInput,
): string {
  const lines: string[] = [];
  lines.push('');
  lines.push(`turns analyzed: ${formatInt(analyzedTurns)}`);
  if (fidelity.excluded.total > 0) {
    lines.push(formatExcludedNote(fidelity));
  }
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

  // Per-model totals. A model that survived the filter with zero turns (e.g.
  // every turn was excluded by --fidelity, or --models pre-seeded a model the
  // user asked about that has no data in the slice) renders the cost as the
  // dash sentinel — not "$0.00", which would falsely claim the model ran for
  // free.
  lines.push('');
  for (const m of t.models) {
    const tot = t.totals[m] ?? { turns: 0, totalCost: 0 };
    const totalCost = tot.turns > 0 ? formatUsd(tot.totalCost) : DASH;
    lines.push(`${displayModelName(m)}: ${formatInt(tot.turns)} turns, ${totalCost} total`);
  }
  lines.push('');
  return lines.join('\n');
}

// "excluded 12 turns below usage-only fidelity (8 aggregate-only, 3 cost-only, 1 partial)"
// — only mention non-zero buckets so the parenthetical stays terse.
function formatExcludedNote(f: FidelityRenderInput): string {
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
  // raw model name, so two providers that ship a model with the same suffix
  // would still aggregate separately and only collide visually in the TTY
  // header. Use --json for the unambiguous identifier.
  const i = m.indexOf('/');
  return i >= 0 ? m.slice(i + 1) : m;
}
