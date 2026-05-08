# CLI golden snapshots

This corpus holds historical 1.x CLI snapshots and a synthetic ledger used by
the Rust CLI golden test. The TypeScript capture scripts have been removed with
the 1.x packages, so these files are now fixed parity fixtures rather than a
regeneratable TS baseline.

## Layout

```text
tests/fixtures/cli-golden/
├── README.md
├── invocations.json      — args + sealed env per snapshot.
├── ledger/               — committed synthetic ledger fixture.
│   ├── burn.sqlite       — 2.x events/stamps database.
│   ├── content.sqlite    — 2.x content/search database.
│   └── ledger.jsonl      — historical bootstrap source retained for tests.
├── project/
│   └── CLAUDE.md         — overhead-eligible file so `burn overhead`
│                           returns non-empty.
└── snapshots/            — historical stdout snapshots and optional stderr.
```

## What's snapshotted

`invocations.json` lists the CLI surfaces the diff runner knows about. The set
covers read-path commands (`summary`, `hotspots`, `overhead`, `overhead trim`,
`compare`, `state status`) in human and JSON forms, plus help text for
action-path commands (`burn ingest --help`, `burn run --help`,
`burn mcp-server --help`) and top-level `burn --help`.

Action-path commands themselves are deliberately not snapshotted: their output
depends on a real spawn lifecycle or watch loop, which cannot be reproduced
from a static ledger.

## Running The Diff Runner

The diff runner lives at `crates/relayburn-cli/tests/golden.rs` and is gated by
`BURN_GOLDEN=1` so ordinary `cargo test --workspace` runs stay fast.

```bash
cargo build --workspace
BURN_GOLDEN=1 cargo test --test golden -- --nocapture
```

Each invocation also has an `enabled` flag. Disabled entries are reported as
skipped; enabled entries spawn the Rust `burn` binary against the fixture ledger
and compare normalized stdout/stderr with `snapshots/`.

## Updating Fixtures

There is no longer a TypeScript recapture path. For a deliberate 2.x behavior
change, update the affected snapshot by running the Rust command manually under
the same sealed environment used by `golden.rs`, then review the diff as a
fixture change.

When adding a new snapshot:

1. Add an entry to `invocations.json` with `enabled: false`.
2. Add the expected snapshot file under `snapshots/`.
3. Flip `enabled: true` in the same PR that implements the matching CLI
   behavior.
