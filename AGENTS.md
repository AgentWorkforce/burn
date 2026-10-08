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

Add query behavior to the Rust SDK/CLI/MCP presenter surface as appropriate.

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

## Quality benchmark

The Quality CI workflow runs [rust-oleum](https://github.com/AgentWorkforce/rust-oleum)
(our own OSS code-quality ratchet, extracted from this repo) against
`rust-oleum.toml` at the repo root: targets for complexity, Halstead
difficulty, lines per file, coverage, CRAP, and dead/redundant code, plus a
grandfathered `[baseline]` of existing violations. CI fails on new
violations or regressions beyond a grandfathered ceiling; PRs additionally
get `cargo-mutants` run over their diff (target: zero surviving mutants in
changed lines).

```bash
cargo install --locked rust-oleum
rust-oleum                              # report + gate
rust-oleum --coverage lcov.info         # with coverage/CRAP
rust-oleum --write-baseline             # regenerate [baseline]
```

When your PR trips the gate, prefer refactoring under the target over adding
baseline entries. If you refactor a grandfathered offender, shrink or delete
its baseline entry. Never raise a ceiling or add a new entry without calling
it out in the PR description.

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

<!-- prpm:snippet:start @agent-relay/merge-train-snippet@1.0.0 -->
## Merging: `trunk` + the `mergeable` label

CI does **not** run on feature branches. It runs only on the `trunk` → `main`
pull request and on pushes to `main`. (Repos whose default branch is not
`main`, e.g. `master`, use that branch wherever this says `main`.) A merge
agent batches ready PRs into `trunk`, gets that one PR green, and merges it.

**When you open a PR**
1. Branch from `trunk` and open the PR with **base `trunk`**, not `main`.
   A PR into `main` from any other branch fails the `Trunk guard` check.
2. No CI runs on your PR, so verify locally before calling it ready: run the
   typecheck, tests and lint this repo uses, and list the exact commands and
   results in the PR body.

**When the PR is ready**
3. Add the label **`mergeable`** once all of these are true:
   - The change is complete and the local checks above pass.
   - Review feedback (human and bot) is addressed or answered.
   - It is not a draft and does not depend on an unmerged PR.
4. Remove `mergeable` if the PR stops being ready (new work, a failing check, a
   blocking question). The label is read live from GitHub on every sweep.

**What you must not do**
- Do not merge your own PR, and never merge into or push to `trunk` or `main`
  directly.
- Do not re-enable CI for feature branches or edit the `trunk` gates in
  `.github/workflows/`.

**The merge agent** sweeps open `mergeable` PRs with base `trunk` about every
10 minutes. It reads each PR's linked sessions (the `Agent Relay sessions`
block in the PR body, then the session summary) for context, merges them into
`trunk`, opens or updates the `trunk` → `main` PR, fixes CI there, merges when
green, and posts a summary. If your PR conflicts with `trunk`, it may ask you
to rebase on `trunk`; do so and keep the label.

> Interim: the sweep worker is not deployed yet. Until it is, a human or a
> designated agent performs the merge-agent steps manually. Labelling is unchanged.
<!-- prpm:snippet:end @agent-relay/merge-train-snippet@1.0.0 -->
