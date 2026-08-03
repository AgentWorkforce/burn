# Agent guide for relayburn

Conventions an agent (or human) needs to know to work productively in this repo.
Pairs with [`README.md`](./README.md) — README is what burn does, this file is
how to work on it.

## Layout

The repo is Rust-first. `crates/` is the source of truth.

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

The npm workspace contains wrappers and platform package manifests only:

```
packages/sdk-node          — @relayburn/sdk Node facade over relayburn-sdk-node.
packages/sdk-node/npm/*    — @relayburn/sdk-<platform> prebuilt native packages.
packages/mcp               — @relayburn/mcp stdio MCP presenter over @relayburn/sdk.
packages/relayburn         — unscoped npm install wrapper exposing `burn`.
packages/relayburn/npm/*   — @relayburn/cli-<platform> prebuilt binary packages.
```

Add query behavior to the Rust SDK/CLI/MCP presenter surface as appropriate;
the npm workspace contains wrappers and native-package manifests only.

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

## Adding ingest support

`burn ingest` owns session import: no flags scans all known session stores
once, `--watch` follows them, and `--hook claude --quiet` handles Claude hook
payloads from stdin. Harness readers and ingest orchestration live under
`crates/relayburn-sdk/src/{reader,ingest}/`; the CLI presenter lives at
`crates/relayburn-cli/src/commands/ingest.rs`.

Add a harness reader to the SDK and include its source root in `IngestRoots`.
Launchers that cannot provide a session ID before spawn use the pending-stamp
API in `crates/relayburn-sdk/src/ingest/pending_stamps.rs`.

## When in doubt

- **Architecture / API surface:** read `README.md`, then
  `crates/relayburn-sdk/src/lib.rs` for the Rust public surface and
  `packages/sdk-node/src/index.d.ts` for the Node facade.
- **CLI commands and flags:** read `crates/relayburn-cli/src/cli.rs` and verify
  the rendered surface with `cargo run -p relayburn-cli -- --help` plus the
  relevant subcommand `--help`. CLI registration expectations live in
  `crates/relayburn-cli/tests/smoke.rs`.
- **Activity classifier rules:** the rule tables (`TEST_PATTERNS`,
  `EDIT_TOOLS`, `TOOL_ALIASES`, etc.) live at
  `crates/relayburn-sdk/src/reader/classifier.rs`. New harness tool names need
  entries in `TOOL_ALIASES`; a new category requires updating
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
  storage layout is `burn.sqlite` plus `content.sqlite`; WAL mode serializes
  concurrent writers and permits concurrent readers.
