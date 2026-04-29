import { strict as assert } from 'node:assert';
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, beforeEach, describe, it } from 'node:test';

interface CliResult {
  code: number | null;
  stdout: string;
  stderr: string;
}

const cliPath = fileURLToPath(new URL('./cli.js', import.meta.url));

async function runCli(argv: string[], env: NodeJS.ProcessEnv): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cliPath, ...argv], {
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk: Buffer) => {
      stdout += chunk.toString('utf8');
    });
    child.stderr.on('data', (chunk: Buffer) => {
      stderr += chunk.toString('utf8');
    });
    child.on('error', reject);
    child.on('exit', (code) => resolve({ code, stdout, stderr }));
  });
}

describe('CLI budget dispatch', () => {
  let tmp: string;
  let tmpHome: string;
  let env: NodeJS.ProcessEnv;

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-cli-dispatch-'));
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-cli-dispatch-home-'));
    env = {
      ...process.env,
      RELAYBURN_HOME: tmp,
      HOME: tmpHome,
    };
  });

  afterEach(async () => {
    await rm(tmp, { recursive: true, force: true });
    await rm(tmpHome, { recursive: true, force: true });
  });

  it('top-level help advertises budget only', async () => {
    const result = await runCli(['--help'], env);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /burn budget/);
    assert.doesNotMatch(result.stdout, /burn limits/);
    assert.doesNotMatch(result.stdout, /^  burn plans/m);
  });

  it('dispatches budget help and nested plans help', async () => {
    const budget = await runCli(['budget', '--help'], env);
    assert.equal(budget.code, 0);
    assert.match(budget.stdout, /burn budget/);
    assert.match(budget.stdout, /--no-forecast/);

    const plans = await runCli(['budget', 'plans', '--help'], env);
    assert.equal(plans.code, 0);
    assert.match(plans.stdout, /burn budget plans/);
    assert.match(plans.stdout, /set-reset-day/);
  });

  it('dispatches nested plan subcommands and rejects old top-level verbs', async () => {
    const add = await runCli(
      ['budget', 'plans', 'add', '--provider', 'claude', '--preset', 'pro'],
      env,
    );
    assert.equal(add.code, 0);
    assert.match(add.stdout, /added claude-pro/);

    const list = await runCli(['budget', 'plans', '--json', '--no-archive'], env);
    assert.equal(list.code, 0);
    assert.equal(JSON.parse(list.stdout).plans[0].usage.plan.id, 'claude-pro');

    const oldLimits = await runCli(['limits', '--help'], env);
    assert.equal(oldLimits.code, 1);
    assert.match(oldLimits.stderr, /unknown command: limits/);

    const oldPlans = await runCli(['plans', '--help'], env);
    assert.equal(oldPlans.code, 1);
    assert.match(oldPlans.stderr, /unknown command: plans/);
  });
});
