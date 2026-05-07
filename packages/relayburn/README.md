# relayburn

Install the [`burn`](https://github.com/AgentWorkforce/burn) CLI globally:

```sh
npm i -g relayburn
```

This package is a thin install wrapper. It declares the per-platform
`@relayburn/cli-<platform>` packages as `optionalDependencies`; npm's
`os` / `cpu` filters install only the one matching your machine, and the
`burn` shim resolves and execs the prebuilt Rust binary it ships. If a
native package is not available, the shim falls back to the generic
TypeScript `@relayburn/cli` package.

```sh
burn --help
burn summary --since 7d
burn hotspots --since 7d
```

Prebuilt platforms: `darwin-arm64`, `darwin-x64`, `linux-arm64-gnu`
(glibc), `linux-x64-gnu` (glibc). Other hosts use the TypeScript CLI
fallback. Windows native binary support is tracked in
[#359](https://github.com/AgentWorkforce/burn/issues/359).

See the project [README](https://github.com/AgentWorkforce/burn#readme)
for full documentation.
