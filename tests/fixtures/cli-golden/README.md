# CLI golden snapshots

This corpus captures the **TS CLI** output across a fixture ledger so the
**Rust CLI** port (#248, Wave 2 in `RUST_PORT_WAVE_PLAN.md`) can golden-diff
against it. It exists so eight Wave 2 fan-out PRs have a stable target to
assert against; today the Rust binary is a stub and the diff runner is a
no-op until Wave 2 flips invocations on one at a time.

## Layout

```
tests/fixtures/cli-golden/
├── README.md             — you are here
├── invocations.json      — args + sealed env per snapshot; the contract
│                           shared between capture-snapshots.mjs and the
│                           Rust diff runner
├── ledger/               — generated synthetic ledger
│   ├── ledger.jsonl      — turns + user_turns + tool_result_events +
│   ├── ledger.idx        —  relationships, hand-built for stable output
│   └── ledger.content.idx
├── project/              — fake project directory
│   └── CLAUDE.md         — overhead-eligible file so `burn overhead`
│                           returns non-empty
├── scripts/
│   ├── build-ledger.mjs        — repopulates `ledger/` from scratch
│   └── capture-snapshots.mjs   — runs every TS-CLI invocation and writes
│                                 normalized stdout/stderr to `snapshots/`
└── snapshots/            — one `<name>.stdout.txt` per invocation; an
                            optional `<name>.stderr.txt` if the command
                            wrote anything to stderr
```

## What's snapshotted

`invocations.json` lists every TS-CLI surface the diff runner knows about.
The current set covers every read-path command (`summary`, `hotspots`,
`overhead`, `overhead trim`, `compare`, `state status`) in both TTY and
`--json` flavors, plus the help text for the action-path commands
(`burn ingest --help`, `burn run --help`, `burn mcp-server --help`) and the
top-level `burn --help`.

Action-path commands themselves are deliberately *not* snapshotted: their
output depends on a real spawn lifecycle (running an agent harness or a
watch loop), which can't be reproduced from a static ledger. Help text is
the proxy.

`burn overhead trim` is captured non-interactively via the regular
`overhead trim` invocation; the TS implementation prints a unified-diff
recommendation to stdout and never enters an interactive flow, so no
special handling is needed.

## Provenance

- **TS commit at capture:** see `git log -1 --format=%H` on the branch this
  PR landed from. The CHANGELOG entry under `[Unreleased]` will name the
  PR (`#248-c`) so future captures can be cross-referenced.
- **Pricing snapshot:** vendored `packages/analyze/pricing/models.dev.json`
  on the same commit. Cost columns in `summary`, `hotspots`, and `compare`
  snapshots only stay stable if pricing doesn't drift.
- **Activity classifier rules:** the fixture ledger sets `activity` on
  every `TurnRecord` directly so the snapshots don't depend on the rule
  tables in `packages/reader/src/classifier.ts`. A classifier-rule change
  *will not* drift these snapshots; re-run capture only if you want the
  fresh classification to flow through.

## Refresh procedure

```bash
# from the repo root, on a clean workspace
pnpm run golden:capture
git -C <worktree> diff tests/fixtures/cli-golden/snapshots
```

Equivalently, without pnpm:

```bash
pnpm run build
node tests/fixtures/cli-golden/scripts/capture-snapshots.mjs
```

The capture script:

1. Wipes `tests/fixtures/cli-golden/ledger/` and rebuilds it via
   `build-ledger.mjs`.
2. For each entry in `invocations.json`, spawns
   `packages/cli/dist/cli.js` with a sealed env:
   - `HOME=<a fresh tmp dir>` so `ingestAll` finds no agent sessions
   - `RELAYBURN_HOME=tests/fixtures/cli-golden/ledger`
   - `RELAYBURN_CONTENT_STORE=off` so no content sidecars are materialized
   - `RELAYBURN_ARCHIVE=0` to force the streaming-ledger fallback (the
     SQLite archive path is a perf optimization the Rust port may not
     have on day one; the streaming path produces identical aggregates)
   - `NO_COLOR=1`, `FORCE_COLOR=0` for stable, ANSI-free output.
3. Writes captured stdout to `snapshots/<name>.stdout.txt` and (if
   non-empty) stderr to `snapshots/<name>.stderr.txt`.
4. Normalizes two classes of machine-specific noise before writing:
   - the absolute fixture HOME path → `${RELAYBURN_HOME}`
   - the absolute fixture project path → `${PROJECT}`
   - wall-clock millisecond fields in `state status --json`
     (`ledgerMtimeMsCurrent`, `lastBuiltAt`, `lastRebuildAt`) → `${MTIME}`
     / `${TS}`
   The Rust diff runner applies the same substitutions before comparing.

## How Wave 2 PRs use this

The diff runner lives at `crates/relayburn-cli/tests/golden.rs`. It is
gated on the env var `BURN_GOLDEN=1` so plain `cargo test --workspace`
in CI stays green while the Rust CLI is being filled in. Per-invocation
gating happens via the `enabled: bool` flag on each entry in
`invocations.json`.

The default state on `main` today is **all `enabled: false`**: the test
runs to completion, prints a "skip <name> (enabled=false)" line for each
invocation, and reports success. As each Wave 2 PR lands its slice of
the Rust CLI, the matching invocations flip to `enabled: true` and the
diff runner starts enforcing parity for them. The mapping is:

| Wave 2 dev | PR scope                                | Flip these enabled flags                                                |
|------------|-----------------------------------------|--------------------------------------------------------------------------|
| D1         | `burn summary` + `burn hotspots`         | `summary`, `summary-json`, `hotspots`, `hotspots-json`                  |
| D2         | `burn overhead` + `burn overhead trim`   | `overhead`, `overhead-json`, `overhead-trim`, `overhead-trim-json`      |
| D3         | `burn compare`                           | `compare`, `compare-json`                                                |
| D4         | `burn state` (status / rebuild / prune)  | `state-status`, `state-status-json`                                      |
| D5         | `burn run` + Claude adapter              | `run-help`, `top-level-help`                                             |
| D6         | Codex adapter                            | (no new help-only snapshot — covered by `top-level-help`)               |
| D7         | OpenCode adapter                         | (no new help-only snapshot — covered by `top-level-help`)               |
| D8         | `burn ingest` + `burn mcp-server`        | `ingest-help`, `mcp-server-help`, `top-level-help`                      |

The expected PR sequence: a Wave 2 dev implements their command, runs
`BURN_GOLDEN=1 cargo test --test golden -- --nocapture` locally, watches
the diff runner pass, flips the matching `enabled: true` in this fixture's
`invocations.json` in the same PR, and re-runs to verify CI stays green.

The very last Wave 2 PR (whichever lands last) should also remove the
`BURN_GOLDEN=1` env-var guard from `crates/relayburn-cli/tests/golden.rs`
so the diff runner runs by default in CI from then on.

## Running the diff runner manually

```bash
# Build the Rust binary first; the integration test references it via
# CARGO_BIN_EXE_burn so cargo handles wiring as long as we go through it.
cargo build --workspace

# Pre-Wave-2: every invocation is enabled=false so this is a fast no-op.
BURN_GOLDEN=1 cargo test --test golden -- --nocapture

# To prove the runner actually fails against a stub: temporarily flip one
# invocation to enabled=true and re-run; you'll get a unified diff between
# the snapshot and the stub binary's "not yet implemented" output. Revert
# the flag before committing.
```

## Adding a new snapshot

1. Add an entry to `invocations.json` with `enabled: false`.
2. Run `pnpm run golden:capture` to regenerate snapshots.
3. Commit the new snapshot file plus the invocations.json change.
4. The Wave 2 PR that owns the matching command flips `enabled: true`.
