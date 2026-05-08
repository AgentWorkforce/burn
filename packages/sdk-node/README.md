# @relayburn/sdk

Embeddable Relayburn SDK for Node.js. The package is a napi-rs facade over the
Rust `relayburn-sdk` crate.

The package resolves the native binding for your platform via
`optionalDependencies`:

| Platform | Package |
|---|---|
| darwin-arm64 (Apple Silicon) | `@relayburn/sdk-darwin-arm64` |
| darwin-x64 (Intel Mac) | `@relayburn/sdk-darwin-x64` |
| linux-arm64-gnu | `@relayburn/sdk-linux-arm64-gnu` |
| linux-x64-gnu | `@relayburn/sdk-linux-x64-gnu` |

Windows (`win32-x64-msvc`) is not yet shipped — see #247 follow-up.

## API Notes

- Large u64 token counts may be `bigint`. napi-rs maps Rust `u64` to
  JavaScript `BigInt`; the facade downcasts safe-range integers to `number`
  and leaves larger values as `bigint`. The declarations widen these fields
  to `number | bigint`.
- The SDK exposes read verbs such as `summary()`, `sessionCost()`,
  `hotspots()`, `compare()`, `search()`, `exportLedger()`, and
  `exportStamps()`.
