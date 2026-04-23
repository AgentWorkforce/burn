# Agent guide for relayburn

Conventions an agent (or human) needs to know to work productively in this repo.
Pairs with [`README.md`](./README.md) — README is what burn does, this file is how to work on it.

## Layout

pnpm workspace, four published packages in dependency order:

```
@relayburn/reader   — pure parsers (Claude Code / Codex / OpenCode session logs → TurnRecord)
@relayburn/ledger   — append-only JSONL ledger + content sidecar at ~/.relayburn/
@relayburn/analyze  — pricing + per-record cost derivation + comparison aggregator
@relayburn/cli      — `burn` binary (summary, by-tool, compare, claude/codex/opencode wrappers, …)
```

`reader → ledger → analyze → cli`. Always build the whole workspace; never touch a single package's `tsconfig.tsbuildinfo` to "skip" a dep.

## Common commands

```bash
pnpm install              # frozen-lockfile install
pnpm run build            # tsc --build across the workspace
pnpm run test             # node --test against built dist/
pnpm run test:ts          # build + test in one shot
pnpm dev:cli <args>       # run the local CLI against a built dist/

pnpm run pricing:update   # refresh the vendored models.dev snapshot
```

Tests run from `dist/` so a stale build will lie. If a test fails unexpectedly, rebuild before debugging.

## Commit messages drive the changelog

The publish workflow (`.github/workflows/publish.yml`) auto-generates per-package CHANGELOG entries by parsing `git log` since the last `<pkg>-v*` tag. **You do not need to manually edit CHANGELOG.md** — the release workflow inserts a new section under `[Unreleased]` at publish time.

It buckets commits by their subject's leading verb. Use one of these to land in the right section:

| Subject starts with… | Lands in section |
|---|---|
| `Add`, `Implement`, `Introduce`, `Create`, `Support`, `Enable`, `Expose`, `Wire`, `Allow` | **Added** |
| `Fix`, `Resolve`, `Correct`, `Patch`, `Prevent`, `Guard`, `Stop` | **Fixed** |
| `Refactor`, `Rename`, `Extract`, `Reorganize`, `Restructure`, `Simplify`, `Move`, `Split`, `Consolidate`, `Rewrite`, `Replace`, `Update`, `Bump`, `Upgrade`, `Migrate`, `Switch`, `Tighten`, `Loosen`, `Tweak`, `Adjust`, `Improve`, `Clarify`, `Polish`, `Cleanup`, `Harden` | **Changed** |
| `Test`, `Cover`, `Verify` | **Reliability** |
| `Document`, `Docs`, `Readme` | **Documentation** |
| anything else | **Changed** (catch-all so nothing is silently dropped) |

Conventional Commits (`feat:`, `fix:`, `refactor:`, `chore(release):`, etc.) also work and take precedence over verb inference. Either style is fine; mixing is fine.

Breaking changes: append `!` to a Conventional Commits prefix (e.g. `feat!:`) to land under **Breaking Changes**.

## Releases

```bash
# from GitHub Actions: workflow_dispatch → "Publish Package"
#   package: all | reader | ledger | analyze | cli
#   version: patch | minor | major | prepatch | … | none (re-publish current)
#   custom_version: 0.3.1 (overrides version type)
#   tag: latest | next | beta | alpha
#   dry_run: true to skip publish + tag + git push
```

The workflow:
1. Builds + tests the whole workspace.
2. Bumps `package.json` versions in dep order (reader → ledger → analyze → cli).
3. Generates changelog entries from `git log <pkg>-v<last>..HEAD -- packages/<pkg>`.
4. Publishes via `pnpm pack` + `npm publish` using OIDC trusted-publisher auth (no `NPM_TOKEN`).
5. Tags `<pkg>-v<version>` and creates a GitHub Release with the changelog body.

A separate `Verify Publish` workflow smoke-tests installs from npm afterward.

## When in doubt

- **Architecture / API surface:** read `README.md` first, then the package's `src/index.ts` for exports.
- **Activity classifier rules:** the rule tables (`TEST_PATTERNS`, `EDIT_TOOLS`, `TOOL_ALIASES`, etc.) live at `packages/reader/src/classifier.ts`. They're the source of truth for what `burn compare` buckets each turn into. Adding a new harness = adding entries to `TOOL_ALIASES`; adding a new category = updating `ActivityCategory` in `packages/reader/src/types.ts` and adding its rule + a test.
- **Ledger schema:** `packages/reader/src/types.ts` (`TurnRecord`, `ContentRecord`) and `packages/ledger/src/schema.ts` (`LedgerLine`, `TurnLine`, `StampLine`). Bump `v` if the on-disk shape changes.
- **Concurrency:** any read-modify-write on the ledger MUST hold `withLock('ledger', …)` from `@relayburn/ledger`. Append-only writes use the same lock to avoid racing reclassify-style rewrites.
