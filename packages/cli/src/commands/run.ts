import { spawn as nodeSpawn } from 'node:child_process';
import type { ChildProcess, SpawnOptions } from 'node:child_process';

import type { ParsedArgs } from '../args.js';
import type { IngestReport } from '../ingest.js';
import {
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from '../spawn-tags.js';

import { listHarnessNames, lookupHarness } from '../harnesses/registry.js';
import type { HarnessAdapter, HarnessRunContext } from '../harnesses/types.js';

const RUN_HELP = `burn run — spawn an agent harness with attribution

Usage:
  burn run <harness> [--tag k=v ...] [-- <harness args>]

Known harnesses: ${listHarnessNames().join(', ')}

Examples:
  burn run claude   --tag workflow=refactor -- --resume
  burn run codex    --tag workflow=refactor
  burn run opencode --tag workflow=refactor
`;

export type SpawnFn = (
  command: string,
  args: readonly string[],
  options: SpawnOptions,
) => ChildProcess;

export interface RunWrapperOptions {
  spawn?: SpawnFn;
}

export async function runWrapper(
  args: ParsedArgs,
  opts: RunWrapperOptions = {},
): Promise<number> {
  const harnessName = args.positional[0];
  if (!harnessName || harnessName === 'help' || args.flags['help'] === true) {
    process.stdout.write(RUN_HELP);
    return harnessName ? 0 : 2;
  }
  const adapter = lookupHarness(harnessName);
  if (!adapter) {
    process.stderr.write(
      `burn: unknown harness "${harnessName}". Known: ${listHarnessNames().join(', ')}\n`,
    );
    return 2;
  }
  return runWithAdapter(adapter, args, opts);
}

export async function runWithAdapter(
  adapter: HarnessAdapter,
  args: ParsedArgs,
  opts: RunWrapperOptions = {},
): Promise<number> {
  const spawn = opts.spawn ?? nodeSpawn;
  const envTags = readSpawnEnvTags();
  const tags = mergeSpawnTags(envTags, args.tags);
  tags['harness'] = adapter.name;
  tags['burnSpawn'] = '1';
  const spawnStartTs = new Date();
  tags['burnSpawnTs'] = spawnStartTs.toISOString();

  const ctx: HarnessRunContext = {
    cwd: process.cwd(),
    passthrough: args.passthrough,
    tags,
    spawnStartTs,
  };

  const plan = await adapter.plan(ctx);
  await adapter.beforeSpawn(ctx, plan);

  let totalIngestedSessions = 0;
  let totalAppendedTurns = 0;
  const onReport = (report: IngestReport): void => {
    totalIngestedSessions += report.ingestedSessions;
    totalAppendedTurns += report.appendedTurns;
  };

  const watcher = adapter.startWatcher?.(ctx, onReport) ?? null;

  const child = spawn(plan.binary, plan.args, {
    stdio: 'inherit',
    env: {
      ...process.env,
      ...spawnTagEnvOverrides(tags),
      ...(plan.envOverrides ?? {}),
    },
  });
  if (watcher) void watcher.tick();

  const code: number = await new Promise((resolve) => {
    child.on('exit', (c) => resolve(c ?? 0));
    child.on('error', (err: Error) => {
      process.stderr.write(`[burn] failed to spawn ${plan.binary}: ${err.message}\n`);
      resolve(127);
    });
  });

  if (watcher) await watcher.stop();
  const finalReport = await adapter.afterExit(ctx, plan);
  totalIngestedSessions += finalReport.ingestedSessions;
  totalAppendedTurns += finalReport.appendedTurns;
  process.stderr.write(
    `[burn] ${adapter.name} ingest: ${totalIngestedSessions} session` +
      `${totalIngestedSessions === 1 ? '' : 's'} ` +
      `(+${totalAppendedTurns} turn${totalAppendedTurns === 1 ? '' : 's'})\n`,
  );
  return code;
}

// Deprecated alias: dispatched when the user invokes `burn claude|codex|opencode`
// directly. Prints a one-line stderr deprecation notice and forwards to the
// driver. Slated for removal in the next minor release.
export async function runDeprecatedAlias(
  harnessName: string,
  args: ParsedArgs,
  opts: RunWrapperOptions = {},
): Promise<number> {
  const adapter = lookupHarness(harnessName);
  if (!adapter) {
    process.stderr.write(
      `burn: unknown harness "${harnessName}". Known: ${listHarnessNames().join(', ')}\n`,
    );
    return 2;
  }
  process.stderr.write(
    `[burn] \`burn ${harnessName}\` is deprecated; use \`burn run ${harnessName}\`. ` +
      `The alias will be removed in the next minor release.\n`,
  );
  return runWithAdapter(adapter, args, opts);
}
