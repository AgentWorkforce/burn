# @relayburn/sdk (2.x)

## [Unreleased]

- Initial scaffolding: umbrella package layout (`@relayburn/sdk`) +
  per-platform packages (`@relayburn/sdk-{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-x64-gnu}`)
  resolved via `optionalDependencies`, TS facade re-exporting the napi-rs
  binding, conformance scaffold against the TS 1.x SDK, esbuild bundle
  smoke test. (#247 part b)
- Shape conformance with TS `@relayburn/sdk@1.x`: `Ledger.open()` returns
  a `Promise<Ledger>` instance, `sessionCost()` emits `totalUSD`
  (screaming USD), every read verb is `async` (`Promise<T>`),
  `IngestOptions` is `{ sessionId, harness, ledgerHome }`, `top` and
  `minSample` accept plain `number`, and `onLog` callbacks are accepted
  on every read verb's options (silently dropped at the napi boundary
  until the SDK wires fallback logging). Adds `search`, `exportLedger`,
  `exportStamps`, `BurnErrorCode`, `OverheadFileKind`, and
  `HotspotsGroupBy` as 2.x extensions over the 1.x surface. (#247 part c)
- Conformance suite is now wired into CI: `napi build` writes its outputs
  (`.node`, `binding.cjs`, `binding.d.ts`) into `src/` so the generated
  loader's local-file branch resolves; the suite seeds a deterministic
  ledger via `tests/fixtures/cli-golden/scripts/build-ledger.mjs` and
  flips `RELAYBURN_SDK_NAPI_BUILT=1` to enable the `deepStrictEqual` gate
  against TS `@relayburn/sdk@1.x`. (#247 part d)
