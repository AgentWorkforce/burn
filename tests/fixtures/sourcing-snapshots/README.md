# Sourcing snapshots

The `SessionRecords` burn derives from each session fixture under
`tests/fixtures/{claude,codex,opencode}`, one JSON file per session. They are
the frozen spec for mapping relayhistory evidence onto burn records:
`source::parity_tests` stages each fixture, syncs it through `ai-hist`, maps
it, and diffs the result against these files. Adopting a new `ai-hist`
version re-runs that suite.

The snapshots record what burn's original session readers derived from each
fixture, except for two edited by hand where burn's records deliberately
differ:

- `opencode-user-turn-blocks-ses_utb.json` — a failed OpenCode tool call sets
  `toolCalls[].isError = true`, as Claude and Codex turns already do.
- `claude-incomplete-then-complete.json` — a Claude assistant turn still in
  progress (no `stop_reason` yet) is not emitted; it becomes a turn once its
  response settles.

The files change only by hand, alongside the mapping change they describe.
