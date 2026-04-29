import { mkdir, readFile, writeFile } from 'node:fs/promises';
import * as path from 'node:path';

import { plansPath } from './paths.js';

export type PlanProvider = 'claude' | 'cursor' | 'custom';

export interface Plan {
  // Stable identifier the user passes to `burn budget plans remove <id>` and
  // `set-reset-day <id>`. Defaults to `<provider>-<preset>` for built-in
  // presets and is required for custom plans.
  id: string;
  provider: PlanProvider;
  // Human-readable label shown in `burn budget plans` and `burn budget` output.
  name: string;
  budgetUsd: number;
  // Day-of-month the cycle resets on (1-31). Day 1 is calendar-month reset.
  // Days >28 clamp to the actual month length when the target month is
  // shorter (e.g. resetDay=31 in February resets on the 28th/29th).
  resetDay: number;
}

export interface PlansFile {
  plans: Plan[];
}

export interface PlanPreset {
  // Key the CLI accepts via `burn budget plans add --provider claude --preset pro`.
  preset: string;
  plan: Plan;
}

// Built-in presets per issue #39. Cursor spend is not currently surfaced by
// any of the readers (see `SourceKind` in `@relayburn/reader`), so registering
// the cursor preset is a UX nicety only — its spend will report as $0 until
// a Cursor adapter lands.
export const BUILTIN_PRESETS: PlanPreset[] = [
  {
    preset: 'claude/pro',
    plan: {
      id: 'claude-pro',
      provider: 'claude',
      name: 'Claude Pro',
      budgetUsd: 20,
      resetDay: 1,
    },
  },
  {
    preset: 'claude/max',
    plan: {
      id: 'claude-max',
      provider: 'claude',
      name: 'Claude Max',
      budgetUsd: 200,
      resetDay: 1,
    },
  },
  {
    preset: 'cursor/pro',
    plan: {
      id: 'cursor-pro',
      provider: 'cursor',
      name: 'Cursor Pro',
      budgetUsd: 20,
      resetDay: 1,
    },
  },
];

export function findPreset(provider: PlanProvider, preset: string): Plan | null {
  const key = `${provider}/${preset}`.toLowerCase();
  const hit = BUILTIN_PRESETS.find((p) => p.preset.toLowerCase() === key);
  return hit ? { ...hit.plan } : null;
}

export async function loadPlans(): Promise<Plan[]> {
  let raw: string;
  try {
    raw = await readFile(plansPath(), 'utf8');
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
    throw err;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    throw new Error(`invalid JSON in ${plansPath()}: ${(err as Error).message}`);
  }
  if (!parsed || typeof parsed !== 'object') {
    throw new Error(`${plansPath()} must contain a JSON object with a "plans" array`);
  }
  const list = (parsed as { plans?: unknown }).plans;
  if (!Array.isArray(list)) {
    throw new Error(`${plansPath()} is missing a "plans" array`);
  }
  return list.map((row, i) => normalizePlan(row, i));
}

export async function savePlans(plans: Plan[]): Promise<void> {
  await mkdir(path.dirname(plansPath()), { recursive: true });
  const body: PlansFile = { plans };
  await writeFile(plansPath(), JSON.stringify(body, null, 2) + '\n', 'utf8');
}

export function normalizePlan(raw: unknown, index: number): Plan {
  if (!raw || typeof raw !== 'object') {
    throw new Error(`plans[${index}] is not an object`);
  }
  const r = raw as Record<string, unknown>;
  const id = pickString(r['id'], `plans[${index}].id`);
  const provider = pickProvider(r['provider'], `plans[${index}].provider`);
  const name = pickString(r['name'], `plans[${index}].name`);
  const budgetUsd = pickPositiveNumber(r['budgetUsd'], `plans[${index}].budgetUsd`);
  const resetDay = pickResetDay(r['resetDay'], `plans[${index}].resetDay`);
  return { id, provider, name, budgetUsd, resetDay };
}

function pickString(v: unknown, label: string): string {
  if (typeof v !== 'string' || v.trim().length === 0) {
    throw new Error(`${label} must be a non-empty string`);
  }
  return v.trim();
}

function pickProvider(v: unknown, label: string): PlanProvider {
  if (v === 'claude' || v === 'cursor' || v === 'custom') return v;
  throw new Error(`${label} must be "claude", "cursor", or "custom"`);
}

function pickPositiveNumber(v: unknown, label: string): number {
  if (typeof v !== 'number' || !Number.isFinite(v) || v <= 0) {
    throw new Error(`${label} must be a positive number`);
  }
  return v;
}

function pickResetDay(v: unknown, label: string): number {
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 1 || v > 31) {
    throw new Error(`${label} must be an integer 1-31`);
  }
  return v;
}
