import { strict as assert } from 'node:assert';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  BUILTIN_PRESETS,
  findPreset,
  loadPlans,
  normalizePlan,
  plansPath,
  savePlans,
} from './index.js';

describe('plans config', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-plans-'));
    process.env['RELAYBURN_HOME'] = tmp;
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('returns [] when plans.json does not exist', async () => {
    const plans = await loadPlans();
    assert.deepEqual(plans, []);
  });

  it('round-trips a saved plans list', async () => {
    const plan = { ...BUILTIN_PRESETS[0]!.plan };
    await savePlans([plan]);
    const loaded = await loadPlans();
    assert.deepEqual(loaded, [plan]);
  });

  it('throws on malformed JSON', async () => {
    await writeFile(plansPath(), 'not json', 'utf8');
    await assert.rejects(loadPlans, /invalid JSON/);
  });

  it('throws when plans.json is not an object with a plans array', async () => {
    await writeFile(plansPath(), '[]', 'utf8');
    await assert.rejects(loadPlans, /missing a "plans" array/);
  });

  it('rejects rows with missing or invalid fields', async () => {
    assert.throws(() => normalizePlan({ id: '', provider: 'claude' }, 0), /id must be a non-empty string/);
    assert.throws(
      () => normalizePlan({ id: 'x', provider: 'foo', name: 'X', budgetUsd: 1, resetDay: 1 }, 0),
      /provider must be/,
    );
    assert.throws(
      () => normalizePlan({ id: 'x', provider: 'claude', name: 'X', budgetUsd: -1, resetDay: 1 }, 0),
      /budgetUsd must be a positive number/,
    );
    assert.throws(
      () => normalizePlan({ id: 'x', provider: 'claude', name: 'X', budgetUsd: 1, resetDay: 32 }, 0),
      /resetDay must be an integer 1-31/,
    );
    assert.throws(
      () => normalizePlan({ id: 'x', provider: 'claude', name: 'X', budgetUsd: 1, resetDay: 1.5 }, 0),
      /resetDay must be an integer 1-31/,
    );
  });
});

describe('findPreset', () => {
  it('returns claude/pro preset with $20 budget', () => {
    const p = findPreset('claude', 'pro');
    assert.ok(p);
    assert.equal(p!.id, 'claude-pro');
    assert.equal(p!.budgetUsd, 20);
  });

  it('returns claude/max preset with $200 budget', () => {
    const p = findPreset('claude', 'max');
    assert.ok(p);
    assert.equal(p!.budgetUsd, 200);
  });

  it('returns cursor/pro preset', () => {
    const p = findPreset('cursor', 'pro');
    assert.ok(p);
    assert.equal(p!.provider, 'cursor');
  });

  it('returns null for unknown preset', () => {
    assert.equal(findPreset('claude', 'bogus'), null);
    assert.equal(findPreset('custom', 'anything'), null);
  });

  it('is case-insensitive', () => {
    assert.ok(findPreset('claude', 'PRO'));
    assert.ok(findPreset('claude', 'Max'));
  });

  it('returns a copy so callers cannot mutate the registry', () => {
    const a = findPreset('claude', 'pro')!;
    a.budgetUsd = 999;
    const b = findPreset('claude', 'pro')!;
    assert.equal(b.budgetUsd, 20);
  });
});
