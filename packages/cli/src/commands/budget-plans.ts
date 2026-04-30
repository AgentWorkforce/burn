import {
  BUILTIN_PRESETS,
  buildArchive,
  findPreset,
  loadPlans,
  openArchive,
  plansPath,
  queryAll,
  savePlans,
} from '@relayburn/ledger';
import type { Plan, PlanProvider } from '@relayburn/ledger';
import {
  computePlanUsage,
  loadPricing,
  planUsageFromArchive,
} from '@relayburn/analyze';
import type { PlanUsage } from '@relayburn/analyze';

import type { ParsedArgs } from '../args.js';
import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, table } from '../format.js';
import { withProgress } from '../progress.js';

const BUDGET_PLANS_HELP = `burn budget plans — monthly quota tracking against your plan budget

Usage:
  burn budget plans                                              list configured plans + status
  burn budget plans add --provider <p> --preset <name>           add a built-in preset
  burn budget plans add --id <id> --provider custom \\
                 --name <"Label"> --budget <usd> \\
                 [--reset-day <1-31>]                     add a custom plan
  burn budget plans remove <id>                                  drop a plan from the list
  burn budget plans set-reset-day <id> <day>                     change a plan's reset day

Built-in presets:
${BUILTIN_PRESETS.map((p) => {
  const note = p.plan.provider === 'cursor' ? ' — spend tracking unavailable' : '';
  return `  ${p.preset.padEnd(14)} ${p.plan.name} ($${p.plan.budgetUsd}/mo, resets day ${p.plan.resetDay})${note}`;
}).join('\n')}

Examples:
  burn budget plans add --provider claude --preset max
  burn budget plans add --id work-api --provider custom --name "Work Anthropic API" --budget 500
  burn budget plans set-reset-day claude-max 15
  burn budget plans remove cursor-pro

Plans are stored at:
  ${plansPath()}
`;

export async function runBudgetPlans(args: ParsedArgs): Promise<number> {
  const sub = args.positional[0];
  if (args.flags['help'] !== undefined || sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(BUDGET_PLANS_HELP);
    return 0;
  }
  if (sub === undefined) return runList(args);
  if (sub === 'add') return runAdd(args);
  if (sub === 'remove') return runRemove(args);
  if (sub === 'set-reset-day') return runSetResetDay(args);
  process.stderr.write(`burn budget plans: unknown subcommand "${sub}"\n\n${BUDGET_PLANS_HELP}`);
  return 2;
}

async function runList(args: ParsedArgs): Promise<number> {
  const json = args.flags['json'] === true;
  const plans = await loadPlans();
  const statuses = await statusForPlans(plans, { useArchive: shouldUseArchive(args) });

  if (json) {
    // Hand-shape the per-plan payload so the `fidelity` block is emitted next
    // to the rest of the cycle stats. Mirrors the shape `burn budget --json`
    // would build if it grew the same field — keep the two surfaces parallel.
    const payload = {
      plans: statuses.map((s) => ({
        usage: {
          ...s.usage,
          fidelity: {
            confidence: s.usage.fidelity.confidence,
            summary: s.usage.fidelity.summary,
          },
        },
      })),
    };
    process.stdout.write(JSON.stringify(payload, null, 2) + '\n');
    return 0;
  }

  if (statuses.length === 0) {
    process.stdout.write(
      'No plans configured. Add one with `burn budget plans add --provider claude --preset pro`.\n',
    );
    return 0;
  }

  const anyLowConfidence = statuses.some((s) => s.usage.fidelity.confidence === 'low');
  const headers = ['id', 'name', 'spent', 'projected', 'budget', 'reset'];
  if (anyLowConfidence) headers.push('confidence');
  const rows: string[][] = [headers];
  for (const s of statuses) {
    const u = s.usage;
    const projected = formatUsd(u.projectedEndOfCycleUsd);
    const projectedCell = u.limitedData ? `${projected} (limited data)` : projected;
    const row = [
      u.plan.id,
      u.plan.name,
      formatUsd(u.spentUsd),
      projectedCell,
      formatUsd(u.plan.budgetUsd),
      `${u.daysElapsed}/${u.daysInCycle} days`,
    ];
    if (anyLowConfidence) {
      row.push(u.fidelity.confidence === 'low' ? 'low (partial token data)' : 'high');
    }
    rows.push(row);
  }
  let output = table(rows) + '\n';
  // When any cycle has at least one turn missing per-turn token coverage,
  // append a footer line that names the worst affected plan so users can
  // tell at a glance whether the totals are a lower bound. Suppressed when
  // every cycle is full-fidelity.
  for (const s of statuses) {
    const u = s.usage;
    if (u.fidelity.confidence !== 'low') continue;
    const total = u.fidelity.summary.total;
    if (total === 0) continue;
    const lacking = countTurnsLackingTokens(u.fidelity.summary);
    if (lacking === 0) continue;
    output +=
      `note: ${u.plan.id}: ${lacking} of ${total} turns this cycle ` +
      `lack per-turn token data — totals are a lower bound.\n`;
  }
  process.stdout.write(output);
  return 0;
}

// Count turns whose per-turn input or output token coverage is missing.
// Mirrors the `confidence === 'low'` rule in `computePlanUsage` so the
// rendered count agrees with the per-plan flag. We approximate using the
// summary's `missingCoverage` counts: any turn missing input *or* output
// counts; we take the max of the two as a safe upper bound (a turn missing
// both still counts once, which is what the user wants to read).
function countTurnsLackingTokens(summary: {
  missingCoverage: { hasInputTokens: number; hasOutputTokens: number };
  byClass: { partial: number; 'aggregate-only': number; 'cost-only': number };
}): number {
  const fromCoverage = Math.max(
    summary.missingCoverage.hasInputTokens,
    summary.missingCoverage.hasOutputTokens,
  );
  // Fallback for records whose granularity already classes them as
  // aggregate-only / cost-only / partial — those are by definition missing
  // per-turn token coverage even if the coverage flags happen to be on.
  const fromClass =
    summary.byClass.partial +
    summary.byClass['aggregate-only'] +
    summary.byClass['cost-only'];
  return Math.max(fromCoverage, fromClass);
}

async function runAdd(args: ParsedArgs): Promise<number> {
  const provider = args.flags['provider'];
  if (typeof provider !== 'string' || !isProvider(provider)) {
    process.stderr.write(
      `burn budget plans add: --provider must be one of claude, cursor, custom\n\n${BUDGET_PLANS_HELP}`,
    );
    return 2;
  }

  const preset = args.flags['preset'];
  let plan: Plan;
  if (typeof preset === 'string') {
    const found = findPreset(provider, preset);
    if (!found) {
      process.stderr.write(
        `burn budget plans add: unknown preset "${provider}/${preset}". Available: ${BUILTIN_PRESETS.map((p) => p.preset).join(', ')}\n`,
      );
      return 2;
    }
    plan = found;
  } else if (provider === 'custom') {
    try {
      plan = customFromFlags(args);
    } catch (err) {
      process.stderr.write(
        `burn budget plans add: ${err instanceof Error ? err.message : String(err)}\n` +
          'pass --preset <name> for a built-in plan, or --provider custom with --id/--name/--budget for a custom one.\n',
      );
      return 2;
    }
  } else {
    process.stderr.write(
      'burn budget plans add: pass --preset <name> for a built-in plan, or --provider custom with --id/--name/--budget for a custom one.\n',
    );
    return 2;
  }

  // Allow flag overrides on top of presets so users can tweak budget / id
  // without writing the JSON file by hand.
  if (typeof args.flags['id'] === 'string') plan.id = args.flags['id'];
  if (typeof args.flags['name'] === 'string') plan.name = args.flags['name'];
  if (typeof args.flags['budget'] === 'string') {
    const n = Number(args.flags['budget']);
    if (!Number.isFinite(n) || n <= 0) {
      process.stderr.write(`burn budget plans add: --budget must be a positive number\n`);
      return 2;
    }
    plan.budgetUsd = n;
  }
  if (typeof args.flags['reset-day'] === 'string') {
    const day = parseResetDay(args.flags['reset-day']);
    if (day === null) {
      process.stderr.write(`burn budget plans add: --reset-day must be an integer 1-31\n`);
      return 2;
    }
    plan.resetDay = day;
  }

  const existing = await loadPlans();
  if (existing.some((p) => p.id === plan.id)) {
    process.stderr.write(
      `burn budget plans add: plan with id "${plan.id}" already exists. Remove it first or pass --id <new-id>.\n`,
    );
    return 2;
  }
  await savePlans([...existing, plan]);
  process.stdout.write(`added ${plan.id}: ${plan.name} ($${plan.budgetUsd}/mo, resets day ${plan.resetDay})\n`);
  if (plan.provider === 'cursor') {
    process.stdout.write(
      'note: Cursor moved usage tracking server-side in early 2026, so this ' +
        "plan's spend will report as $0 until Cursor exposes a local data source again.\n",
    );
  }
  return 0;
}

function customFromFlags(args: ParsedArgs): Plan {
  const requireString = (key: string): string => {
    const v = args.flags[key];
    if (typeof v !== 'string' || v.length === 0) {
      throw new Error(`--${key} is required for custom plans`);
    }
    return v;
  };
  const requireBudget = (): number => {
    const v = args.flags['budget'];
    if (typeof v !== 'string') throw new Error('--budget is required for custom plans');
    const n = Number(v);
    if (!Number.isFinite(n) || n <= 0) throw new Error('--budget must be a positive number');
    return n;
  };
  const id = requireString('id');
  const name = requireString('name');
  const budgetUsd = requireBudget();
  const resetDayRaw = args.flags['reset-day'];
  const resetDay = typeof resetDayRaw === 'string' ? parseResetDay(resetDayRaw) ?? 1 : 1;
  return { id, provider: 'custom', name, budgetUsd, resetDay };
}

async function runRemove(args: ParsedArgs): Promise<number> {
  const id = args.positional[1];
  if (!id) {
    process.stderr.write(`burn budget plans remove: missing plan id\n\n${BUDGET_PLANS_HELP}`);
    return 2;
  }
  const plans = await loadPlans();
  const next = plans.filter((p) => p.id !== id);
  if (next.length === plans.length) {
    process.stderr.write(`burn budget plans remove: no plan with id "${id}"\n`);
    return 1;
  }
  await savePlans(next);
  process.stdout.write(`removed ${id}\n`);
  return 0;
}

async function runSetResetDay(args: ParsedArgs): Promise<number> {
  const id = args.positional[1];
  const dayArg = args.positional[2];
  if (!id || !dayArg) {
    process.stderr.write(
      `burn budget plans set-reset-day: usage: burn budget plans set-reset-day <id> <1-31>\n`,
    );
    return 2;
  }
  const day = parseResetDay(dayArg);
  if (day === null) {
    process.stderr.write(`burn budget plans set-reset-day: <day> must be an integer 1-31, got ${dayArg}\n`);
    return 2;
  }
  const plans = await loadPlans();
  const idx = plans.findIndex((p) => p.id === id);
  if (idx === -1) {
    process.stderr.write(`burn budget plans set-reset-day: no plan with id "${id}"\n`);
    return 1;
  }
  plans[idx] = { ...plans[idx]!, resetDay: day };
  await savePlans(plans);
  process.stdout.write(`updated ${id}: resets on day ${day}\n`);
  return 0;
}

export interface PlanStatus {
  usage: PlanUsage;
}

export interface LoadPlanStatusesOptions {
  quiet?: boolean;
}

interface StatusForPlansOptions {
  /**
   * When true, aggregate spend with one SQL query per plan against the
   * archive (`archive.sqlite`). When false (default), use the legacy
   * `queryAll()` + in-memory reduce path. Defaults to `false` so callers
   * have to opt in explicitly: `burn budget plans` wires this to `shouldUseArchive`
   * (the `--no-archive` flag + `RELAYBURN_ARCHIVE` env var); `burn budget`
   * stays on the legacy path until it's migrated separately.
   */
  useArchive?: boolean;
  quiet?: boolean;
}

export async function loadPlanStatuses(
  args?: ParsedArgs,
  opts: LoadPlanStatusesOptions = {},
): Promise<PlanStatus[]> {
  const plans = await loadPlans();
  return statusForPlans(plans, {
    useArchive: args ? shouldUseArchive(args) : false,
    ...progressOptions(opts),
  });
}

// Shared by `burn budget plans` (list view) and `burn budget` (composite view)
// so both surfaces show identical numbers.
async function statusForPlans(
  plans: Plan[],
  opts: StatusForPlansOptions = {},
): Promise<PlanStatus[]> {
  if (plans.length === 0) return [];
  const progress = progressOptions(opts);
  await withProgress(
    'ingesting latest sessions',
    (task) => ingestAll({ onProgress: (message) => task.update(`ingest: ${message}`) }),
    progress,
  );
  const pricing = await withProgress('loading pricing snapshot', async (task) => {
    const loaded = await loadPricing();
    task.succeed('loaded pricing snapshot');
    return loaded;
  }, progress);
  const useArchive = opts.useArchive ?? false;

  if (useArchive) {
    // Materialize the ledger tail into the archive once before any plan
    // queries so `SELECT SUM(...) FROM turns` sees every turn the legacy
    // `queryAll()` path would have. Cheap when up to date (idempotent).
    await withProgress('updating archive', async (task) => {
      const result = await buildArchive();
      task.succeed(
        `updated archive: ${formatInt(result.turnsApplied)} turn` +
          `${result.turnsApplied === 1 ? '' : 's'} applied`,
      );
    }, progress);
    const db = await openArchive();
    try {
      const now = new Date();
      return plans.map((plan) => ({
        usage: planUsageFromArchive(plan, { pricing, db, now }),
      }));
    } finally {
      db.close();
    }
  }

  // Fallback: in-memory reduce. Walk the ledger once across the widest cycle
  // window so we still beat per-plan re-scanning when several plans share a
  // common cycle.
  const oldestStart = plans
    .map((p) => {
      const usageStub = computePlanUsage(p, [], { pricing, now: new Date() });
      return usageStub.cycleStart.getTime();
    })
    .reduce((a, b) => Math.min(a, b), Date.now());
  const since = new Date(oldestStart).toISOString();
  const turns = await withProgress('reading ledger turns', async (task) => {
    const rows = await queryAll({ since });
    task.succeed(`read ${formatInt(rows.length)} turn${rows.length === 1 ? '' : 's'}`);
    return rows;
  }, progress);
  return plans.map((plan) => ({ usage: computePlanUsage(plan, turns, { pricing }) }));
}

function progressOptions(opts: { quiet?: boolean }): { quiet?: boolean } {
  return opts.quiet === undefined ? {} : { quiet: opts.quiet };
}

/**
 * `--no-archive` flag (or `RELAYBURN_ARCHIVE=0`) opts back into the legacy
 * `queryAll()` reduce path. Kept while we shake out the archive migration.
 */
function shouldUseArchive(args: ParsedArgs): boolean {
  if (args.flags['no-archive'] === true) return false;
  const env = process.env['RELAYBURN_ARCHIVE'];
  if (env === '0' || env === 'false') return false;
  return true;
}

function isProvider(s: string): s is PlanProvider {
  return s === 'claude' || s === 'cursor' || s === 'custom';
}

function parseResetDay(s: string): number | null {
  const n = Number(s);
  if (!Number.isInteger(n) || n < 1 || n > 31) return null;
  return n;
}
