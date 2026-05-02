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

describe('CLI command dispatch', () => {
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

  it('top-level help does not advertise removed budget verbs', async () => {
    const result = await runCli(['--help'], env);
    assert.equal(result.code, 0);
    assert.doesNotMatch(result.stdout, /burn budget/);
    assert.doesNotMatch(result.stdout, /burn limits/);
    assert.doesNotMatch(result.stdout, /^  burn plans/m);
  });

  it('rejects removed budget and legacy top-level verbs', async () => {
    const budget = await runCli(['budget', '--help'], env);
    assert.equal(budget.code, 1);
    assert.match(budget.stderr, /unknown command: budget/);

    const oldLimits = await runCli(['limits', '--help'], env);
    assert.equal(oldLimits.code, 1);
    assert.match(oldLimits.stderr, /unknown command: limits/);

    const oldPlans = await runCli(['plans', '--help'], env);
    assert.equal(oldPlans.code, 1);
    assert.match(oldPlans.stderr, /unknown command: plans/);
  });
});
