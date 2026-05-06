# @relayburn/sdk (2.x)

## [Unreleased]

- Initial scaffolding: umbrella package layout (`@relayburn/sdk`) +
  per-platform packages (`@relayburn/sdk-{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-x64-gnu}`)
  resolved via `optionalDependencies`, TS facade re-exporting the napi-rs
  binding, conformance scaffold against the TS 1.x SDK, esbuild bundle
  smoke test. (#247 part b)
