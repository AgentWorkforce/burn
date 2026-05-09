# Agent guide for relayburn

Conventions an agent (or human) needs to know to work productively in this repo.
Pairs with [`README.md`](./README.md) — README is what burn does, this file is
how to work on it.

## Layout

The repo is Rust-first. The old TypeScript 1.x implementation packages have
been removed; `crates/` is the source of truth.

### Rust crates (`crates/`)

Only `relayburn-sdk` and `relayburn-cli` are published to crates.io. Crate
names are prefixed `relayburn-*` because `burn` is taken on crates.io; the
binary keeps the `burn` invocation via `[[bin]] name = "burn"` in
`relayburn-cli`.

```
relayburn-sdk         — PUBLISHED to crates.io; embedding API.
                          src/{reader,ledger,analyze,ingest}/ are internal modules.
                          The public verb surface lives in
                          src/{query_verbs,export_verbs,ingest_verb}.rs.
relayburn-cli         — PUBLISHED to crates.io; produces the `burn` binary.
                          Consumes the SDK as an external embedder would.
relayburn-sdk-node    — napi-rs bindings; built in CI to produce
                          @relayburn/sdk .node artifacts. Not published to crates.io.
```

Build order is `relayburn-sdk -> relayburn-cli`, with `relayburn-sdk-node` also
depending on `relayburn-sdk`. Toolchain is pinned in `rust-toolchain.toml` at
the repo root.

Every new read verb should land first in `relayburn-sdk` as a pure function or
`LedgerHandle` method. The CLI and MCP presenter surfaces should wrap SDK calls
rather than duplicating query logic.

### npm packages (`packages/`)

The npm workspace now contains wrappers and platform package manifests only:

```
packages/sdk-node          — @relayburn/sdk Node facade over relayburn-sdk-node.
packages/sdk-node/npm/*    — @relayburn/sdk-<platform> prebuilt native packages.
packages/mcp               — @relayburn/mcp stdio MCP presenter over @relayburn/sdk.
packages/relayburn         — unscoped npm install wrapper exposing `burn`.
packages/relayburn/npm/*   — @relayburn/cli-<platform> prebuilt binary packages.
```

Do not recreate the old standalone reader/ledger/analyze/ingest/cli TypeScript
packages. If a 1.x feature is missing from 2.x, add it to the Rust SDK/CLI/MCP
presenter surface as appropriate.

## Common commands

```bash
cargo build --workspace    # Build all Rust crates.
cargo test --workspace     # Rust unit/integration tests.

pnpm install               # Workspace install for npm wrappers.
pnpm run test              # Node SDK facade + MCP tests.
pnpm run test:bundle       # esbuild smoke test for @relayburn/sdk.
pnpm run build:napi        # Local napi-rs build for @relayburn/sdk.

pnpm run pricing:update    # Refresh the vendored models.dev snapshot.
```

When debugging CLI behavior locally, prefer the Rust binary:

```bash
cargo run -p relayburn-cli -- summary --since 24h
```

Terminology note: the old `waste` / `diagnose` names are now `hotspots`, and
the old `context` / `context advise` surface is now `overhead` /
`overhead trim`. Do not add compatibility aliases for the old names.

## Changelog

Curate `[Unreleased]` in the relevant changelog as you land PRs:

- `CHANGELOG.md` for cross-package or user-facing release narrative.
- `packages/sdk-node/CHANGELOG.md` for the Node SDK facade.
- `packages/mcp/CHANGELOG.md` for the MCP package.
- `packages/relayburn/CHANGELOG.md` for the npm CLI install wrapper.

Changelog entries should be concise and impact-first. Prefer one short bullet
per user-visible change: name the command/API/schema touched and the practical
effect. Drop issue/PR links, internal review notes, implementation backstory,
and "foundation for..." phrasing unless that text clearly explains the shipped
impact.

## Releases

```bash
# from GitHub Actions: workflow_dispatch -> "Publish Packages"
#   version: patch | minor | major | prepatch | … | none (re-publish current)
#   custom_version: 0.3.1 (overrides version type)
#   tag: latest | next | beta | alpha
#   dry_run: true to skip publish + tag + git push
```

The workflow builds and tests the Rust workspace, builds native artifacts for
the npm platform packages, publishes the umbrellas (`relayburn`,
`@relayburn/sdk`, `@relayburn/mcp`) and their optional dependencies, then tags
each published target.

## Adding a harness

`burn run <harness>` dispatches through a `HarnessAdapter` registered in
`crates/relayburn-cli/src/harnesses/registry.rs`. Adding a new harness is a
new adapter module plus a registration entry.

Key files:

- `crates/relayburn-cli/src/harnesses/mod.rs` — trait definitions and shared
  harness types.
- `crates/relayburn-cli/src/harnesses/registry.rs` — lazy adapter lookup and
  `list_harness_names()`.
- `crates/relayburn-cli/src/harnesses/pending_stamp.rs` — shared shape for
  harnesses that need pending-stamp manifests and a watch loop.

The CLI help block reads `list_harness_names()` so it updates automatically.

`burn ingest` owns passive ingest modes: no flags scans all session stores
once, `--watch` keeps polling, and `--hook claude --quiet` is the stdin-driven
Claude hook path. The reusable polling controller lives at
`crates/relayburn-sdk/src/ingest/watch_loop.rs`.

## When in doubt

- **Architecture / API surface:** read `README.md`, then
  `crates/relayburn-sdk/src/lib.rs` for the Rust public surface and
  `packages/sdk-node/src/index.d.ts` for the Node facade.
- **Activity classifier rules:** the rule tables (`TEST_PATTERNS`,
  `EDIT_TOOLS`, `TOOL_ALIASES`, etc.) live at
  `crates/relayburn-sdk/src/reader/classifier.rs`. Adding a new harness means
  adding entries to `TOOL_ALIASES`; adding a new category means updating
  `ActivityCategory` in `crates/relayburn-sdk/src/reader/types.rs` and adding
  its rule plus tests.
- **Derived state commands:** status, rebuild targets, and content pruning live
  under `burn state` in `crates/relayburn-cli/src/commands/state.rs`. Keep
  maintenance verbs there rather than adding new top-level CLI dispatch.
- **Ledger schema:** `crates/relayburn-sdk/src/reader/types.rs` defines
  `TurnRecord` / content record shapes and
  `crates/relayburn-sdk/src/ledger/schema.rs` defines the SQLite layout. Bump
  schema/versioning deliberately when the on-disk shape changes.
- **Concurrency:** use the SDK ledger APIs and SQLite transactions. The 2.x
  steady-state layout is `burn.sqlite` plus `content.sqlite` in WAL mode; do
  not reintroduce JSONL file-lock write paths.
