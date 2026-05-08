# @relayburn/sdk (2.x)

Embeddable Relayburn SDK — napi-rs bindings over the Rust `relayburn-sdk`
crate.

The 2.x umbrella resolves the right native binary for your platform via
`optionalDependencies`:

| Platform | Package |
|---|---|
| darwin-arm64 (Apple Silicon) | `@relayburn/sdk-darwin-arm64` |
| darwin-x64 (Intel Mac) | `@relayburn/sdk-darwin-x64` |
| linux-arm64-gnu | `@relayburn/sdk-linux-arm64-gnu` |
| linux-x64-gnu | `@relayburn/sdk-linux-x64-gnu` |

Windows (`win32-x64-msvc`) is not yet shipped — see #247 follow-up.

## Migration From 1.x

Same imports, same option shapes, same return shapes — except:

- **Large u64 token counts may be `bigint`.** napi-rs maps Rust `u64` to
  JavaScript `BigInt`; the facade downcasts safe-range integers to `number`
  and leaves larger values as `bigint`. The declarations widen these fields
  to `number | bigint`.
- 2.x also exposes `search()`, `exportLedger()`, and `exportStamps()` as Rust
  SDK extensions.

## Status

The Rust SDK is the in-repo source of truth. The removed TypeScript 1.x
package is tracked only by the compatibility notes in
`RUST_2X_GAP_CATALOG.md`.
