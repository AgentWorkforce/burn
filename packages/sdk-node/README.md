# @relayburn/sdk (2.x)

Embeddable Relayburn SDK — napi-rs bindings over the Rust `relayburn-sdk`
crate. Drop-in replacement for the TS `@relayburn/sdk@1.x` published from
`packages/sdk/`.

The 2.x umbrella resolves the right native binary for your platform via
`optionalDependencies`:

| Platform | Package |
|---|---|
| darwin-arm64 (Apple Silicon) | `@relayburn/sdk-darwin-arm64` |
| darwin-x64 (Intel Mac) | `@relayburn/sdk-darwin-x64` |
| linux-arm64-gnu | `@relayburn/sdk-linux-arm64-gnu` |
| linux-x64-gnu | `@relayburn/sdk-linux-x64-gnu` |

Windows (`win32-x64-msvc`) is not yet shipped — see #247 follow-up.

## Migration from 1.x

Same imports, same option shapes, same return shapes — except:

- **u64 token counts are `bigint`.** napi-rs maps Rust `u64` to JavaScript
  `BigInt`. Code that does arithmetic on `summary().totalTokens` (and
  similar fields on `hotspots`, `overhead`, `sessionCost`) needs to either
  use `BigInt` literals (`100n`) or coerce with `Number(x)`. The TS
  declarations widen these fields to `number | bigint` to keep existing
  callers compiling.
- Otherwise byte-for-byte compatible. Run your test suite — the conformance
  test in `test/conformance.test.js` is what we use to validate.

## Status

This is a `2.0.0-pre` build published to npm under the `next` tag while
the rest of the Rust port lands. Until the lockstep 2.0 cutover ships, the
1.x TS SDK at `packages/sdk/` is still the source of truth.
