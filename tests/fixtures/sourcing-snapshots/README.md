# Sourcing snapshots

The `SessionRecords` burn derives from each session fixture under
`tests/fixtures/{claude,codex,opencode}`, one JSON file per session. They are
the spec for mapping relayhistory evidence onto burn records:
`source::parity_tests` stages each fixture, syncs it through `ai-hist`, maps
it, and diffs the result against these files.

The snapshots were generated from burn's builtin (pre-relayhistory) session
readers with `UPDATE_SOURCING_SNAPSHOTS=1 cargo test -p relayburn-sdk
sourcing_snapshots`, and `source::snapshot_tests` still holds those readers to
them — except for two snapshots edited by hand, where burn's records
deliberately differ from what the builtin readers produced:

- `opencode-user-turn-blocks-ses_utb.json` — a failed OpenCode tool call sets
  `toolCalls[].isError = true`, as Claude and Codex turns already do.
- `claude-incomplete-then-complete.json` — a Claude assistant turn still in
  progress (no `stop_reason` yet) is not emitted; it becomes a turn once its
  response settles.

Both are listed in `DELIBERATE_DEVIATIONS` in `snapshot_tests.rs`, so an
update run leaves them as written.
