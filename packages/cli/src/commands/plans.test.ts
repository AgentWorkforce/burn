import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { appendTurns, loadPlans, savePlans } from '@relayburn/ledger';
import type { Plan } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import type { ParsedArgs } from '../args.js';
import { runPlans, statusForPlans } from './plans.js';

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
  let tmpHome: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalHome = process.env['HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-plans-cli-'));
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-plans-cli-home-'));
    process.env['RELAYBURN_HOME'] = tmp;
    // Isolate `homedir()` so `ingestAll`'s scans of ~/.claude/projects,
    // ~/.codex/sessions, and ~/.local/share/opencode/storage land in an
    // empty temp dir — the dev's real session data must not leak into the
    // test ledger and contaminate parity numbers.
    process.env['HOME'] = tmpHome;
    delete process.env['RELAYBURN_ARCHIVE'];
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmp, { recursive: true, force: true });
    await rm(tmpHome, { recursive: true, force: true });
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

  // --- archive-vs-fallback parity (issue #91) -----------------------------
  //
  // The migration to `planUsageFromArchive` must produce byte-identical
  // output to the legacy `queryAll()` reduce path. We pin the parity here
  // through `runPlans` so the CLI surface (text + `--json`) is exercised
  // end-to-end, not just the analyze-layer helper.

  function turn(opts: {
    ts: string;
    inputTokens?: number;
    outputTokens?: number;
    source?: TurnRecord['source'];
    model?: string;
    sessionId?: string;
    messageId?: string;
  }): TurnRecord {
    return {
      v: 1,
      source: opts.source ?? 'claude-code',
      sessionId: opts.sessionId ?? 's-parity',
      messageId: opts.messageId ?? `m-${opts.ts}`,
      turnIndex: 0,
      ts: opts.ts,
      model: opts.model ?? 'claude-sonnet-4-5',
      usage: {
        input: opts.inputTokens ?? 0,
        output: opts.outputTokens ?? 0,
        reasoning: 0,
        cacheRead: 0,
        cacheCreate5m: 0,
        cacheCreate1h: 0,
      },
      toolCalls: [],
    };
  }

  // Build a fixture that straddles the cycle boundary, irrespective of the
  // wall-clock day this test runs on. We anchor turns to "yesterday" /
  // "two days ago" relative to `now` (so they always land in the current
  // resetDay=1 cycle), plus a turn one day before the cycle start (which
  // both code paths must exclude identically).
  function fixtureTurnsForNow(now: Date): TurnRecord[] {
    const yesterday = new Date(now.getTime() - 1 * 24 * 60 * 60 * 1000);
    const twoDaysAgo = new Date(now.getTime() - 2 * 24 * 60 * 60 * 1000);
    // First-of-the-cycle anchor — stamp a turn that's clearly inside the
    // current cycle even on the 1st of the month.
    const cycleStart = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1));
    const cycleStartPlusHour = new Date(cycleStart.getTime() + 60 * 60 * 1000);
    // One day before cycle start — must be excluded from current cycle.
    const beforeCycle = new Date(cycleStart.getTime() - 24 * 60 * 60 * 1000);
    return [
      turn({ ts: cycleStartPlusHour.toISOString(), inputTokens: 1_000_000, messageId: 'm-cycle-anchor' }),
      turn({ ts: twoDaysAgo.toISOString(), inputTokens: 500_000, messageId: 'm-two-days' }),
      turn({ ts: yesterday.toISOString(), inputTokens: 250_000, messageId: 'm-yesterday' }),
      // Excluded — last cycle's spend should not influence either path.
      turn({ ts: beforeCycle.toISOString(), inputTokens: 9_999_999, messageId: 'm-prev-cycle' }),
    ];
  }

  async function seedPlansAndTurns(plans: Plan[]): Promise<void> {
    await savePlans(plans);
    await appendTurns(fixtureTurnsForNow(new Date()));
  }

  it('list output is byte-identical between archive and --no-archive paths', async () => {
    const claudePro: Plan = {
      id: 'claude-pro',
      provider: 'claude',
      name: 'Claude Pro',
      budgetUsd: 20,
      resetDay: 1,
    };
    await seedPlansAndTurns([claudePro]);

    const archiveRun = await captureStdio(() => runPlans(args([])));
    const fallbackRun = await captureStdio(() => runPlans(args([], { 'no-archive': true })));

    assert.equal(archiveRun.result, 0);
    assert.equal(fallbackRun.result, 0);
    assert.equal(
      archiveRun.stdout,
      fallbackRun.stdout,
      'archive path must render the same table as the queryAll fallback',
    );
  });

  it('--json output is byte-identical between archive and --no-archive paths', async () => {
    const claudePro: Plan = {
      id: 'claude-pro',
      provider: 'claude',
      name: 'Claude Pro',
      budgetUsd: 20,
      resetDay: 1,
    };
    const customWork: Plan = {
      id: 'work-api',
      provider: 'custom',
      name: 'Work Anthropic API',
      budgetUsd: 500,
      resetDay: 1,
    };
    await seedPlansAndTurns([claudePro, customWork]);

    const archiveRun = await captureStdio(() => runPlans(args([], { json: true })));
    const fallbackRun = await captureStdio(() =>
      runPlans(args([], { json: true, 'no-archive': true })),
    );

    assert.equal(archiveRun.result, 0);
    assert.equal(fallbackRun.result, 0);

    const archiveJson = JSON.parse(archiveRun.stdout);
    const fallbackJson = JSON.parse(fallbackRun.stdout);
    // Drop Date-typed fields that JSON.stringify renders as ISO strings —
    // they round-trip identically anyway, but this is the level the issue
    // calls out: "Output is byte-identical to the pre-migration
    // implementation."
    assert.deepEqual(archiveJson, fallbackJson);
    // Spot-check that we exercised both plans, not just an empty list.
    // JSON shape is `{ plans: [{ usage: { plan: {...}, spentUsd, ... } }] }`.
    assert.equal(archiveJson.plans.length, 2);
    assert.ok(
      archiveJson.plans.find(
        (p: { usage: { plan: { id: string } } }) => p.usage.plan.id === 'claude-pro',
      ),
    );
    assert.ok(
      archiveJson.plans.find(
        (p: { usage: { plan: { id: string } } }) => p.usage.plan.id === 'work-api',
      ),
    );
  });

  it('RELAYBURN_ARCHIVE=0 env knob is honored as the queryAll fallback', async () => {
    const claudePro: Plan = {
      id: 'claude-pro',
      provider: 'claude',
      name: 'Claude Pro',
      budgetUsd: 20,
      resetDay: 1,
    };
    await seedPlansAndTurns([claudePro]);

    // Run with the env knob set; compare to the explicit `--no-archive` run.
    process.env['RELAYBURN_ARCHIVE'] = '0';
    const envRun = await captureStdio(() => runPlans(args([], { json: true })));
    delete process.env['RELAYBURN_ARCHIVE'];
    const flagRun = await captureStdio(() =>
      runPlans(args([], { json: true, 'no-archive': true })),
    );

    assert.equal(envRun.result, 0);
    assert.equal(flagRun.result, 0);
    assert.deepEqual(JSON.parse(envRun.stdout), JSON.parse(flagRun.stdout));
  });

  it('statusForPlans archive path materializes the same spentUsd as the fallback', async () => {
    // Direct statusForPlans parity check — bypasses CLI table formatting and
    // pins the underlying number, so a regression in `planUsageFromArchive`'s
    // SUM/GROUP BY can't hide behind table whitespace.
    const claudePro: Plan = {
      id: 'claude-pro',
      provider: 'claude',
      name: 'Claude Pro',
      budgetUsd: 20,
      resetDay: 1,
    };
    await seedPlansAndTurns([claudePro]);

    const archiveStatus = await statusForPlans([claudePro], { useArchive: true });
    const fallbackStatus = await statusForPlans([claudePro], { useArchive: false });

    assert.equal(archiveStatus.length, 1);
    assert.equal(fallbackStatus.length, 1);
    assert.equal(archiveStatus[0]!.usage.spentUsd, fallbackStatus[0]!.usage.spentUsd);
    assert.equal(
      archiveStatus[0]!.usage.projectedEndOfCycleUsd,
      fallbackStatus[0]!.usage.projectedEndOfCycleUsd,
    );
    assert.equal(archiveStatus[0]!.usage.daysElapsed, fallbackStatus[0]!.usage.daysElapsed);
    assert.equal(archiveStatus[0]!.usage.daysInCycle, fallbackStatus[0]!.usage.daysInCycle);
    assert.equal(archiveStatus[0]!.usage.limitedData, fallbackStatus[0]!.usage.limitedData);
  });
});
