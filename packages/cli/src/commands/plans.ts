import {
  BUILTIN_PRESETS,
  findPreset,
  loadPlans,
  plansPath,
  queryAll,
  savePlans,
} from '@relayburn/ledger';
import type { Plan, PlanProvider } from '@relayburn/ledger';
import { computePlanUsage, loadPricing } from '@relayburn/analyze';
import type { PlanUsage } from '@relayburn/analyze';

import type { ParsedArgs } from '../args.js';
import { ingestAll } from '../ingest.js';
import { formatUsd, table } from '../format.js';

const PLANS_HELP = `burn plans — monthly quota tracking against your plan budget

Usage:
  burn plans                                              list configured plans + status
  burn plans add --provider <p> --preset <name>           add a built-in preset
  burn plans add --id <id> --provider custom \\
                 --name <"Label"> --budget <usd> \\
                 [--reset-day <1-31>]                     add a custom plan
  burn plans remove <id>                                  drop a plan from the list
  burn plans set-reset-day <id> <day>                     change a plan's reset day

Built-in presets:
${BUILTIN_PRESETS.map((p) => {
  const note = p.plan.provider === 'cursor' ? ' — spend tracking unavailable (see #22)' : '';
  return `  ${p.preset.padEnd(14)} ${p.plan.name} ($${p.plan.budgetUsd}/mo, resets day ${p.plan.resetDay})${note}`;
}).join('\n')}

Examples:
  burn plans add --provider claude --preset max
  burn plans add --id work-api --provider custom --name "Work Anthropic API" --budget 500
  burn plans set-reset-day claude-max 15
  burn plans remove cursor-pro

Plans are stored at:
  ${plansPath()}
`;

export async function runPlans(args: ParsedArgs): Promise<number> {
  const sub = args.positional[0];
  if (sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(PLANS_HELP);
    return 0;
  }
  if (sub === undefined) return runList(args);
  if (sub === 'add') return runAdd(args);
  if (sub === 'remove') return runRemove(args);
  if (sub === 'set-reset-day') return runSetResetDay(args);
  process.stderr.write(`burn plans: unknown subcommand "${sub}"\n\n${PLANS_HELP}`);
  return 2;
}

async function runList(args: ParsedArgs): Promise<number> {
  const json = args.flags['json'] === true;
  const plans = await loadPlans();
  const statuses = await statusForPlans(plans);

  if (json) {
    process.stdout.write(JSON.stringify({ plans: statuses }, null, 2) + '\n');
    return 0;
  }

  if (statuses.length === 0) {
    process.stdout.write(
      'No plans configured. Add one with `burn plans add --provider claude --preset pro`.\n',
    );
    return 0;
  }

  const rows: string[][] = [['id', 'name', 'spent', 'projected', 'budget', 'reset']];
  for (const s of statuses) {
    const u = s.usage;
    const projected = formatUsd(u.projectedEndOfCycleUsd);
    const notes = [];
    if (u.limitedData) notes.push('limited data');
    if (u.partialData) notes.push('partial fidelity');
    const projectedCell = notes.length > 0 ? `${projected} (${notes.join(', ')})` : projected;
    rows.push([
      u.plan.id,
      u.plan.name,
      formatUsd(u.spentUsd),
      projectedCell,
      formatUsd(u.plan.budgetUsd),
      `${u.daysElapsed}/${u.daysInCycle} days`,
    ]);
  }
  process.stdout.write(table(rows) + '\n');
  return 0;
}

async function runAdd(args: ParsedArgs): Promise<number> {
  const provider = args.flags['provider'];
  if (typeof provider !== 'string' || !isProvider(provider)) {
    process.stderr.write(
      `burn plans add: --provider must be one of claude, cursor, custom\n\n${PLANS_HELP}`,
    );
    return 2;
  }

  const preset = args.flags['preset'];
  let plan: Plan;
  if (typeof preset === 'string') {
    const found = findPreset(provider, preset);
    if (!found) {
      process.stderr.write(
        `burn plans add: unknown preset "${provider}/${preset}". Available: ${BUILTIN_PRESETS.map((p) => p.preset).join(', ')}\n`,
      );
      return 2;
    }
    plan = found;
  } else if (provider === 'custom') {
    try {
      plan = customFromFlags(args);
    } catch (err) {
      process.stderr.write(
        `burn plans add: ${err instanceof Error ? err.message : String(err)}\n` +
          'pass --preset <name> for a built-in plan, or --provider custom with --id/--name/--budget for a custom one.\n',
      );
      return 2;
    }
  } else {
    process.stderr.write(
      'burn plans add: pass --preset <name> for a built-in plan, or --provider custom with --id/--name/--budget for a custom one.\n',
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
      process.stderr.write(`burn plans add: --budget must be a positive number\n`);
      return 2;
    }
    plan.budgetUsd = n;
  }
  if (typeof args.flags['reset-day'] === 'string') {
    const day = parseResetDay(args.flags['reset-day']);
    if (day === null) {
      process.stderr.write(`burn plans add: --reset-day must be an integer 1-31\n`);
      return 2;
    }
    plan.resetDay = day;
  }

  const existing = await loadPlans();
  if (existing.some((p) => p.id === plan.id)) {
    process.stderr.write(
      `burn plans add: plan with id "${plan.id}" already exists. Remove it first or pass --id <new-id>.\n`,
    );
    return 2;
  }
  await savePlans([...existing, plan]);
  process.stdout.write(`added ${plan.id}: ${plan.name} ($${plan.budgetUsd}/mo, resets day ${plan.resetDay})\n`);
  if (plan.provider === 'cursor') {
    process.stdout.write(
      'note: Cursor moved usage tracking server-side in early 2026 (see #22), so this ' +
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
    process.stderr.write(`burn plans remove: missing plan id\n\n${PLANS_HELP}`);
    return 2;
  }
  const plans = await loadPlans();
  const next = plans.filter((p) => p.id !== id);
  if (next.length === plans.length) {
    process.stderr.write(`burn plans remove: no plan with id "${id}"\n`);
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
      `burn plans set-reset-day: usage: burn plans set-reset-day <id> <1-31>\n`,
    );
    return 2;
  }
  const day = parseResetDay(dayArg);
  if (day === null) {
    process.stderr.write(`burn plans set-reset-day: <day> must be an integer 1-31, got ${dayArg}\n`);
    return 2;
  }
  const plans = await loadPlans();
  const idx = plans.findIndex((p) => p.id === id);
  if (idx === -1) {
    process.stderr.write(`burn plans set-reset-day: no plan with id "${id}"\n`);
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

// Shared by `burn plans` (list view) and `burn limits` (composite view) so
// both surfaces show identical numbers.
export async function statusForPlans(plans: Plan[]): Promise<PlanStatus[]> {
  if (plans.length === 0) return [];
  await ingestAll();
  const pricing = await loadPricing();
  // Pull the widest cycle window across plans so we only walk the ledger
  // once. Cheaper than per-plan queryAll for users with several plans.
  const oldestStart = plans
    .map((p) => {
      const usageStub = computePlanUsage(p, [], { pricing, now: new Date() });
      return usageStub.cycleStart.getTime();
    })
    .reduce((a, b) => Math.min(a, b), Date.now());
  const since = new Date(oldestStart).toISOString();
  const turns = await queryAll({ since });
  return plans.map((plan) => ({ usage: computePlanUsage(plan, turns, { pricing }) }));
}

function isProvider(s: string): s is PlanProvider {
  return s === 'claude' || s === 'cursor' || s === 'custom';
}

function parseResetDay(s: string): number | null {
  const n = Number(s);
  if (!Number.isInteger(n) || n < 1 || n > 31) return null;
  return n;
}
