import type { Enrichment } from '@relayburn/ledger';

// Spawner-owned tagging contract. An orchestrator that
// invokes `burn run <harness>` can attach workflow/agent context via
// either:
//
//   --tag k=v ...                (explicit, wins)
//   RELAYBURN_<KEY> env vars     (implicit, transitively inherited)
//
// Both feed the same stamp bag, so attribution is uniform across the three
// harnesses. When the wrapper spawns its child, the merged values are
// re-exported via env so a transitive `burn …` invocation inside the child
// session inherits the same context without the orchestrator having to
// re-thread it.
//
// Keep this list explicit — accepting arbitrary `RELAYBURN_*` env vars would
// shadow internals like `RELAYBURN_HOME` / `RELAYBURN_SESSION_ID`. New
// orchestrator fields go here, and only here.
export const SPAWN_ENV_TAG_KEYS: ReadonlyArray<{ env: string; tag: string }> = [
  { env: 'RELAYBURN_WORKFLOW_ID', tag: 'workflowId' },
  { env: 'RELAYBURN_STEP_ID', tag: 'stepId' },
  { env: 'RELAYBURN_AGENT_ID', tag: 'agentId' },
  { env: 'RELAYBURN_PARENT_AGENT_ID', tag: 'parentAgentId' },
  { env: 'RELAYBURN_PERSONA', tag: 'persona' },
  { env: 'RELAYBURN_TIER', tag: 'tier' },
];

// Read RELAYBURN_* spawn-tag env vars from `env` (or `process.env` by default)
// into the Enrichment shape used by `stamp`. Empty strings are dropped — they
// usually mean "key was unset by a parent wrapper" and would otherwise pollute
// the stamp.
export function readSpawnEnvTags(
  env: NodeJS.ProcessEnv = process.env,
): Enrichment {
  const out: Enrichment = {};
  for (const { env: envName, tag } of SPAWN_ENV_TAG_KEYS) {
    const v = env[envName];
    if (typeof v === 'string' && v.length > 0) out[tag] = v;
  }
  return out;
}

// Merge env-derived spawn tags with explicit `--tag k=v` flags. CLI flags win
// on key collision: they are the more-explicit signal at the moment of spawn,
// while env vars represent transitively inherited defaults.
export function mergeSpawnTags(
  envTags: Enrichment,
  cliTags: Enrichment,
): Enrichment {
  return { ...envTags, ...cliTags };
}

// Build the env block to hand to the child harness so a nested `burn …`
// invocation inside it inherits the same spawn-tag context. We re-export every
// final tag value (env-derived OR CLI-derived) under its canonical RELAYBURN_*
// name, so the inner wrapper sees the merged bag — not just whatever the outer
// orchestrator originally set. Other RELAYBURN_* variables (HOME, SESSION_ID,
// CONTENT_*) flow through untouched via `...env` at the call site.
export function spawnTagEnvOverrides(
  finalTags: Enrichment,
): Record<string, string> {
  const overrides: Record<string, string> = {};
  for (const { env: envName, tag } of SPAWN_ENV_TAG_KEYS) {
    const v = finalTags[tag];
    if (typeof v === 'string' && v.length > 0) overrides[envName] = v;
  }
  return overrides;
}
