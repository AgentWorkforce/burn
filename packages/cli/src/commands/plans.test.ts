import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { loadPlans } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import { runPlans } from './plans.js';

function args(positional: string[] = [], flags: Record<string, string | true> = {}): ParsedArgs {
  return { positional, flags, tags: {}, passthrough: [] };
}

async function captureStdio<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; stdout: string; stderr: string }> {
  let stdout = '';
  let stderr = '';
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  process.stdout.write = ((c: string | Uint8Array) => {
    stdout += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((c: string | Uint8Array) => {
    stderr += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stderr.write;
  try {
    const result = await fn();
    return { result, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

describe('burn plans CLI', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-plans-cli-'));
    process.env['RELAYBURN_HOME'] = tmp;
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('list with no plans points the user at `add`', async () => {
    const { result, stdout } = await captureStdio(() => runPlans(args()));
    assert.equal(result, 0);
    assert.match(stdout, /No plans configured/);
    assert.match(stdout, /burn plans add/);
  });

  it('add --provider claude --preset max persists the plan', async () => {
    const { result } = await captureStdio(() =>
      runPlans(args(['add'], { provider: 'claude', preset: 'max' })),
    );
    assert.equal(result, 0);
    const plans = await loadPlans();
    assert.equal(plans.length, 1);
    assert.equal(plans[0]!.id, 'claude-max');
    assert.equal(plans[0]!.budgetUsd, 200);
    assert.equal(plans[0]!.resetDay, 1);
  });

  it('add accepts --reset-day override on a preset', async () => {
    const { result } = await captureStdio(() =>
      runPlans(args(['add'], { provider: 'claude', preset: 'pro', 'reset-day': '15' })),
    );
    assert.equal(result, 0);
    const plans = await loadPlans();
    assert.equal(plans[0]!.resetDay, 15);
  });

  it('rejects --reset-day outside 1-31', async () => {
    const { result, stderr } = await captureStdio(() =>
      runPlans(args(['add'], { provider: 'claude', preset: 'pro', 'reset-day': '99' })),
    );
    assert.equal(result, 2);
    assert.match(stderr, /reset-day must be an integer 1-31/);
  });

  it('add --provider custom without --id surfaces a clear error', async () => {
    const { result, stderr } = await captureStdio(() =>
      runPlans(args(['add'], { provider: 'custom' })),
    );
    assert.equal(result, 2);
    assert.match(stderr, /--id is required/);
  });

  it('add --provider custom with required flags writes a custom plan', async () => {
    const { result } = await captureStdio(() =>
      runPlans(
        args(['add'], {
          provider: 'custom',
          id: 'work-api',
          name: 'Work Anthropic API',
          budget: '500',
          'reset-day': '7',
        }),
      ),
    );
    assert.equal(result, 0);
    const plans = await loadPlans();
    assert.equal(plans[0]!.id, 'work-api');
    assert.equal(plans[0]!.provider, 'custom');
    assert.equal(plans[0]!.budgetUsd, 500);
    assert.equal(plans[0]!.resetDay, 7);
  });

  it('refuses to add a plan with a duplicate id', async () => {
    await captureStdio(() => runPlans(args(['add'], { provider: 'claude', preset: 'pro' })));
    const { result, stderr } = await captureStdio(() =>
      runPlans(args(['add'], { provider: 'claude', preset: 'pro' })),
    );
    assert.equal(result, 2);
    assert.match(stderr, /already exists/);
  });

  it('remove drops the plan', async () => {
    await captureStdio(() => runPlans(args(['add'], { provider: 'claude', preset: 'max' })));
    const { result } = await captureStdio(() => runPlans(args(['remove', 'claude-max'])));
    assert.equal(result, 0);
    const plans = await loadPlans();
    assert.equal(plans.length, 0);
  });

  it('remove with unknown id exits 1 with a clear message', async () => {
    const { result, stderr } = await captureStdio(() =>
      runPlans(args(['remove', 'does-not-exist'])),
    );
    assert.equal(result, 1);
    assert.match(stderr, /no plan with id "does-not-exist"/);
  });

  it('set-reset-day updates the day in place', async () => {
    await captureStdio(() => runPlans(args(['add'], { provider: 'claude', preset: 'max' })));
    const { result } = await captureStdio(() => runPlans(args(['set-reset-day', 'claude-max', '15'])));
    assert.equal(result, 0);
    const plans = await loadPlans();
    assert.equal(plans[0]!.resetDay, 15);
  });

  it('set-reset-day rejects non-integer days', async () => {
    await captureStdio(() => runPlans(args(['add'], { provider: 'claude', preset: 'pro' })));
    const { result, stderr } = await captureStdio(() =>
      runPlans(args(['set-reset-day', 'claude-pro', 'tuesday'])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /must be an integer 1-31/);
  });

  it('unknown subcommand exits 2 with help', async () => {
    const { result, stderr } = await captureStdio(() => runPlans(args(['noop'])));
    assert.equal(result, 2);
    assert.match(stderr, /unknown subcommand "noop"/);
  });
});
