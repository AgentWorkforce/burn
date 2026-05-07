#!/usr/bin/env node
// Platform-resolving spawner for the `burn` Rust binary. Mirrors the
// `@relayburn/sdk` napi-rs dispatcher pattern (see
// `packages/sdk-node/src/binding.cjs`): the umbrella `relayburn` package
// declares the per-platform packages plus the generic `@relayburn/cli`
// fallback as `optionalDependencies`, npm installs the matching native
// package when available, and this script `require.resolve`s the native
// `burn` binary first, then falls back to the TS CLI when needed.
//
// The actual binaries live in `packages/relayburn/npm/<platform>/bin/burn`
// and are dropped there by the cli-build CI matrix at publish time. They
// are gitignored at rest.
//
// ESM is fine here — the umbrella declares `"type": "module"` and engines
// pins Node >=22.

import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';

const require = createRequire(import.meta.url);

// Map (process.platform, process.arch) → platform-package short string.
// Linux glibc-vs-musl detection follows the same `process.report` probe
// the napi-rs loader uses; we only ship glibc artifacts today, so a musl
// host falls through to the generic CLI fallback below.
function detectShort() {
  const { platform, arch } = process;

  if (platform === 'darwin' && arch === 'arm64') return 'darwin-arm64';
  if (platform === 'darwin' && arch === 'x64') return 'darwin-x64';
  if (platform === 'linux' && arch === 'arm64' && !isMusl()) return 'linux-arm64-gnu';
  if (platform === 'linux' && arch === 'x64' && !isMusl()) return 'linux-x64-gnu';

  // Forward-compat for #359 (Windows). The win32-x64 package is not
  // published yet, so resolution will fail with the same actionable
  // error path as any other unsupported platform — but the mapping is
  // here so #359 only needs to add a matrix leg + optionalDependency,
  // not touch this dispatcher.
  if (platform === 'win32' && arch === 'x64') return 'win32-x64';

  return null;
}

function isMusl() {
  if (!process.report) return false;
  try {
    const { glibcVersionRuntime } = (process.report.getReport() || {}).header || {};
    return !glibcVersionRuntime;
  } catch {
    return false;
  }
}

function binSuffix() {
  return process.platform === 'win32' ? '.exe' : '';
}

const short = detectShort();
const fallbackSpecifier = '@relayburn/cli/dist/cli.js';
const passthroughArgs = process.argv.slice(2);

function formatError(err) {
  return err && err.message ? err.message : String(err);
}

function writeResolveFailure(prebuiltError, fallbackError) {
  if (!short) {
    process.stderr.write(
      `relayburn: unsupported prebuilt platform ${process.platform}-${process.arch}.\n` +
        `Supported prebuilt packages: darwin-arm64, darwin-x64, linux-arm64-gnu (glibc), linux-x64-gnu (glibc).\n` +
        `Track native Windows support at https://github.com/AgentWorkforce/burn/issues/359.\n` +
        `Tried generic TypeScript CLI fallback \`${fallbackSpecifier}\`, but it was not installed.\n` +
        `Reinstall \`relayburn\` with optional dependencies enabled or install \`@relayburn/cli\`.\n` +
        `\nFallback resolution error: ${formatError(fallbackError)}\n`,
    );
    return;
  }

  const pkg = `@relayburn/cli-${short}`;
  process.stderr.write(
    `relayburn: failed to resolve prebuilt \`burn\` binary for ${short}.\n` +
      `Expected the optional dependency \`${pkg}\` to be installed; it ships the binary\n` +
      `at \`bin/burn${binSuffix()}\`. This usually means \`npm install\` skipped the optional\n` +
      `dependency (e.g. \`--no-optional\`, a lockfile pinned without it, or an unsupported\n` +
      `platform filter).\n` +
      `Tried generic TypeScript CLI fallback \`${fallbackSpecifier}\`, but it also failed.\n` +
      `Reinstall \`relayburn\` without \`--no-optional\` and try again.\n` +
      `\nPrebuilt resolution error: ${formatError(prebuiltError)}\n` +
      `Fallback resolution error: ${formatError(fallbackError)}\n`,
  );
}

let command = null;
let childArgs = passthroughArgs;
let prebuiltError = null;

if (short) {
  const pkg = `@relayburn/cli-${short}`;
  const binSpecifier = `${pkg}/bin/burn${binSuffix()}`;
  try {
    command = require.resolve(binSpecifier);
  } catch (err) {
    prebuiltError = err;
  }
}

if (!command) {
  try {
    command = process.execPath;
    childArgs = [require.resolve(fallbackSpecifier), ...passthroughArgs];
  } catch (fallbackError) {
    writeResolveFailure(prebuiltError, fallbackError);
    process.exit(1);
  }
}

const child = spawnSync(command, childArgs, {
  stdio: 'inherit',
  windowsHide: false,
});

if (child.error) {
  process.stderr.write(`relayburn: failed to spawn \`burn\`: ${child.error.message}\n`);
  process.exit(1);
}

// Propagate signal exits the same way Node's own child-process docs
// recommend — POSIX shells map signal-terminated children to
// `128 + signo`, and many CI environments key off that exact code.
if (child.signal) {
  // `os.constants.signals` is the Node-side mapping, but it's keyed by
  // signal name and we have it in hand already; defer to the standard
  // 128+signo formula via `process.kill` fallback for unknown names.
  const { constants } = await import('node:os');
  const signo = constants.signals[child.signal];
  process.exit(typeof signo === 'number' ? 128 + signo : 1);
}

process.exit(child.status ?? 1);
