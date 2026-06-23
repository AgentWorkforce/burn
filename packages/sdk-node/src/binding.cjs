// Native-binding loader. At publish time, `napi build` (via `@napi-rs/cli`)
// regenerates this file to dispatch to the right per-platform package
// (`@relayburn/sdk-darwin-arm64`, `@relayburn/sdk-linux-x64-gnu`, etc.) based
// on `process.platform` + `process.arch` + libc detection. The generated
// version pulls the prebuilt `.node` file out of `optionalDependencies` so
// installs don't need a Rust toolchain.
//
// **File extension note:** this file is `.cjs` (not `.js`) because the
// umbrella package is `"type": "module"`, which would make Node treat a
// bare `.js` as ESM and reject the `module.exports` below at load time.
// `napi build` is invoked with `--js src/binding.cjs` (see
// `package.json` scripts + `.github/workflows/napi-build.yml`) so the
// regeneration writes back to the `.cjs` path; both `src/index.js`
// (ESM facade) and `src/index.cjs` (CJS facade) `require('./binding.cjs')`.
//
// This stub matches the napi-rs-generated dispatcher *shape* so the umbrella
// package's TS facade (`src/index.js`) can import from it during local dev /
// CI conformance scaffolding before the prebuilt binaries exist. While
// #247-a is in flight, we throw a clear "binding not built" error instead of
// requiring `*.node` artifacts that don't exist yet.
//
// Once `napi build` runs in CI for the first time, this file is overwritten;
// see `.github/workflows/napi-build.yml`.

const { existsSync, readFileSync } = require('node:fs');
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
  // Prefer the optional-dep platform package; fall back to a sibling .node
  // that `napi build --release` drops next to this loader during local dev.
  const localPaths = localFile
    ? [join(__dirname, localFile), join(__dirname, '..', localFile)]
    : [];
  for (const localPath of localPaths) {
    if (existsSync(localPath)) {
      try {
        return require(localPath);
      } catch (e) {
        loadError = e;
      }
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
  // Surface a clear actionable error. While #247-a is still merging, this is
  // the failure mode CI / dev machines will hit; the conformance test
  // `test/conformance.test.js` checks for it and skips so the suite stays
  // green until bindings land.
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
