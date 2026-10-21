# Sourcing snapshots

The `SessionRecords` burn derives from each session fixture under
`tests/fixtures/{claude,codex,opencode,copilot}`, one JSON file per session. They are
the frozen spec for mapping relayhistory evidence onto burn records:
`source::parity_tests` stages each fixture in a throwaway HOME (its
`/tmp/project` cwd fields pointed at an empty directory there, so nothing on
the host changes project resolution), syncs it through `ai-hist`, maps it,
and diffs the result against these files. A snapshot with no mapped session,
or a mapped session with no snapshot, fails the suite. Adopting a new `ai-hist`
version re-runs that suite.

The snapshots record what burn's original session readers derived from each
fixture, except where burn's records deliberately differ:

- `opencode-user-turn-blocks-ses_utb.json` — a failed OpenCode tool call sets
  `toolCalls[].isError = true`, as Claude and Codex turns already do.
- `claude-incomplete-then-complete.json` — a Claude assistant turn still in
  progress (no `stop_reason` yet) is not emitted; it becomes a turn once its
  response settles.
- Every `codex-*.json` — Codex accounts per model request (#504). A Codex
  turn is still one task, and now records `requestCount`: how many of its
  `token_count` snapshots moved the running total, one per API call. The
  records gain `inferences`, one row per request (`requestId`
  `{messageId}:request:{n:06}`, `requestIdSource: "row-synthetic"`) carrying
  the spend between its snapshot and the one before, the tool calls written
  since that snapshot, and the span from the request's first record to its
  snapshot. A turn's inferences sum to its usage. Claude and OpenCode
  records carry neither field. The other Codex fixtures make one request per
  task, so these files differ from the readers' output only by those two
  fields. `codex-many-requests-one-turn.json` (one task, four requests, and
  a repeated snapshot that is not a request) has no reader counterpart.

The `copilot-*` snapshots have no original reader behind them: they are burn's
mapping of the Copilot CLI journals relayhistory captures, reviewed when the
source was added. `copilot-cli-usage.json` bills each `session.shutdown`'s
growth in Copilot's per-model running totals as `per-session-aggregate` turns,
so its turns sum to the final `modelMetrics`.

The files change only by hand, alongside the mapping change they describe.
