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

## Changelog

Curate `[Unreleased]` in the relevant per-package `packages/*/CHANGELOG.md` as you land PRs — write the entry the way you'd want it to read in a release note. At publish time, the workflow (`.github/workflows/publish.yml`) **promotes** your `[Unreleased]` block verbatim into `## [x.y.z] - DATE` and resets `[Unreleased]` to empty. No double-writing, no post-release hand-editing.

The root `CHANGELOG.md` is the cross-package narrative. Packages release in lockstep, so each release in the root file is a single `## [x.y.z] - YYYY-MM-DD` header that applies to all four packages — no `**Versions:** ...` lines, no per-bullet `[reader, cli]` tags. Update `[Unreleased]` only when the work spans packages or warrants a top-level summary; single-package work belongs only in that package's CHANGELOG.

The publish workflow promotes the root `[Unreleased]` block the same way it does per-package files: at release time it stamps `## [x.y.z] - DATE` (using `max` of the versions bumped in the run) and resets `[Unreleased]` to empty. **No git-log fallback for the root file** — an empty `[Unreleased]` at release time means "no narrative-worthy changes this release" and the file is left alone. So if you want the root to record a release, write the bullet under `[Unreleased]` *before* the publish run.

**Fallback — git-log inference (per-package CHANGELOGs only).** If a package's `[Unreleased]` block is empty at release time, the workflow reconstructs an entry for `packages/<pkg>/CHANGELOG.md` from `git log` subjects since the last `<pkg>-v*` tag. This is only a safety net; prefer hand-curated entries. **The root `CHANGELOG.md` does not get this fallback** — see the previous paragraph. The inference buckets by leading verb:

| Subject starts with… | Lands in section |
|---|---|
| `Add`, `Implement`, `Introduce`, `Create`, `Support`, `Enable`, `Expose`, `Wire`, `Allow` | **Added** |
| `Fix`, `Resolve`, `Correct`, `Patch`, `Prevent`, `Guard`, `Stop` | **Fixed** |
| `Refactor`, `Rename`, `Extract`, `Reorganize`, `Restructure`, `Simplify`, `Move`, `Split`, `Consolidate`, `Rewrite`, `Replace`, `Update`, `Bump`, `Upgrade`, `Migrate`, `Switch`, `Tighten`, `Loosen`, `Tweak`, `Adjust`, `Improve`, `Clarify`, `Polish`, `Cleanup`, `Harden` | **Changed** |
| `Test`, `Cover`, `Verify` | **Reliability** |
| `Document`, `Docs`, `Readme` | **Documentation** |
| anything else | **Changed** (catch-all so nothing is silently dropped) |

Conventional Commits (`feat:`, `fix:`, `refactor:`, `chore(release):`, etc.) also work and take precedence over verb inference. Either style is fine; mixing is fine.

Breaking changes: append `!` to a Conventional Commits prefix (e.g. `feat!:`) to land under **Breaking Changes** (fallback path only — for curated entries, write the section yourself).

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
