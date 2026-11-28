# Working in burn

```sh
pnpm install
pnpm run verify
pnpm run test:bundle
```

- Session acquisition and parsing belong to relayhistory. Consume its `ai-hist::SessionStore`; fix harness quirks upstream and keep frozen sourcing-parity fixtures meaningful.
- Put new read behavior in the Rust SDK; CLI, Node and MCP surfaces wrap it. npm packages are native wrappers, not an independent implementation.
- Keep ledger and schema changes deliberate. Use SDK ledger APIs and transactions for concurrent writes.
- Do not silently raise rust-oleum baselines; call out any new ceiling in the PR. Curate the relevant package and root Unreleased changelogs.

## Agency and guidance

- Act autonomously on routine assigned work; ask forgiveness rather than permission. Preserve explicit production, release and credential gates.
- For guidance, ask the engineering leads, the agent that spawned you, or the layered owner on Agent Relay.
- Only a truly pressing or blocking question goes to `khaliq-gant` or `will-washburn` (CODEOWNERS) on Agent Relay.

## Cross-repo context

- Before and during changes, understand how this repo fits the wider system; consult its repo map and related-repo docs where available.
- Check relevant agents’ summaries on Agent Relay and proactively contact owning agents about dependencies, adjacent changes and work in flight.

## Pull requests

- Open PRs ready for review, never as drafts. If merging requires Khaliq’s OK, use `merge-train:hold`.
- Immediately subscribe to PR feedback through Agent Relay using [this skill](https://github.com/AgentWorkforce/skills/blob/main/skills/subscribing-relay-webhooks-and-writebacks/SKILL.md), and verify the subscription.
- Evaluate every finding. Address applicable feedback and explain declined changes; a PR with unaddressed feedback or conflicts is not ready.
- When ready, loudly alert the CODEOWNERS and the agent that spawned you on Agent Relay.
- Keep related older PRs moving by contacting their author agent; if unavailable, escalate to the CODEOWNERS or layered owner.
