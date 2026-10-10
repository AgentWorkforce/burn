import { createRequire } from 'node:module';
import { dirname, join, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const NAPI_READY = process.env.RELAYBURN_SDK_NAPI_BUILT === '1';
const IN_CI = Boolean(process.env.CI);
const SDK_SRC = join(__dirname, '..', '..', 'src');

export async function loadNapiSdk(t) {
  if (!NAPI_READY) {
    if (IN_CI) {
      throw new Error(
        'RELAYBURN_SDK_NAPI_BUILT=1 is required in CI; run pnpm run build:napi first',
      );
    }
    t.skip('napi-rs binding not built; set RELAYBURN_SDK_NAPI_BUILT=1');
    return null;
  }

  // When the caller claims the binding is ready, a missing or unloadable
  // artifact is a test failure rather than another silent skip.
  const sdk = await import(join(SDK_SRC, 'index.js'));
  const loadedNodeModules = Object.keys(require.cache).filter((path) => path.endsWith('.node'));
  const localPrefix = `${SDK_SRC}${sep}`;
  if (!loadedNodeModules.some((path) => path.startsWith(localPrefix))) {
    throw new Error(
      `Node SDK conformance must load the locally built binding under ${SDK_SRC}; ` +
        `loaded native modules: ${loadedNodeModules.join(', ') || '(none)'}`,
    );
  }
  return sdk;
}
