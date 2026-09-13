// Native-binding loader. Dispatches to a local napi build when present, then
// falls back to the matching per-platform package
// (`@relayburn/sdk-darwin-arm64`, `@relayburn/sdk-linux-x64-gnu`, etc.) based
// on `process.platform` + `process.arch` + libc detection. Published installs
// pull the prebuilt `.node` file out of `optionalDependencies`, so consumers
// don't need a Rust toolchain.
//
// **File extension note:** this file is `.cjs` (not `.js`) because the
// umbrella package is `"type": "module"`, which would make Node treat a
// bare `.js` as ESM and reject the `module.exports` below at load time.
// Both `src/index.js` (ESM facade) and `src/index.cjs` (CJS facade)
// `require('./binding.cjs')`.
//
// This hand-written dispatcher matches the napi-rs loader shape while keeping
// a clear error for fresh checkouts where neither a local build nor a
// platform package is available.
//
// `napi build ... src` emits `src/index.<target>.node` next to this loader;
// see the package scripts and `.github/workflows/napi-build.yml`.

const { existsSync } = require('node:fs');
const { join } = require('node:path');
const { platform, arch } = process;

// Detect glibc vs musl on Linux. napi-rs generates this with `detect-libc`
// at build time; we keep a minimal fallback so `require('./binding.cjs')`
// doesn't crash when run before the binary build.
function isMusl() {
  if (!process.report) return false;
  try {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const { glibcVersionRuntime } = (process.report.getReport() || {}).header || {};
    return !glibcVersionRuntime;
  } catch (_) {
    return false;
  }
}

let nativeBinding = null;
let loadError = null;

function tryRequire(specifier, localFile) {
  // Prefer the sibling .node emitted by a local build; published installs
  // fall back to the optional-dependency platform package.
  const localPath = localFile ? join(__dirname, localFile) : null;
  if (localPath && existsSync(localPath)) {
    try {
      return require(localPath);
    } catch (e) {
      loadError = e;
    }
  }
  try {
    return require(specifier);
  } catch (e) {
    loadError = e;
    return null;
  }
}

if (platform === 'darwin' && arch === 'arm64') {
  nativeBinding = tryRequire('@relayburn/sdk-darwin-arm64', 'index.darwin-arm64.node');
} else if (platform === 'darwin' && arch === 'x64') {
  nativeBinding = tryRequire('@relayburn/sdk-darwin-x64', 'index.darwin-x64.node');
} else if (platform === 'linux' && arch === 'arm64' && !isMusl()) {
  nativeBinding = tryRequire('@relayburn/sdk-linux-arm64-gnu', 'index.linux-arm64-gnu.node');
} else if (platform === 'linux' && arch === 'x64' && !isMusl()) {
  nativeBinding = tryRequire('@relayburn/sdk-linux-x64-gnu', 'index.linux-x64-gnu.node');
}

if (!nativeBinding) {
  // Surface a clear actionable error for fresh local checkouts and broken
  // optional-dependency installs.
  const detail = loadError
    ? `\nUnderlying error: ${loadError.message}`
    : '';
  throw new Error(
    `@relayburn/sdk: native binding not found for ${platform}-${arch}.\n` +
    `Expected one of @relayburn/sdk-{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-x64-gnu} ` +
    `to be installed via optionalDependencies, or a sibling .node prebuilt by ` +
    `\`pnpm --filter @relayburn/sdk run build:napi\`.${detail}`,
  );
}

module.exports = nativeBinding;
