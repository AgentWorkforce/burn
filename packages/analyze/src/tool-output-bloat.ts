// Oversized tool-output bloat detector. Closes #168.
//
// Two signal sources unified under one detector shape:
//
//  - Signal A (Claude-only static config): read `~/.claude/settings.json` and
//    the project's `.claude/settings.json`. The setting itself is in
//    characters, so the parsed value is converted to tokens via the same
//    `bytes/4` heuristic Signal B uses before comparing against the
//    token-unit threshold (default 15000 tokens ≈ 60000 chars). The fix is
//    the config knob itself.
//
//  - Signal B (cross-harness session-data evidence): for every session, find
//    `tool_result` events whose payload exceeds a threshold (default
//    `max(15000 tokens, p95 of observed tool_result tokens)`). Aggregate by
//    `(source, toolName)` so detectors flag tools that consistently produce
//    oversized output across many sessions, not single one-offs.
//
// Both signals emit the same `ToolOutputBloat` shape so the CLI can render a
// single severity-ranked list. `cost` is the rough USD waste attributed to
// carrying the oversized payload — Signal B uses per-call `approxTokens` from
// user-turn blocks (content-sidecar enrichment), while Signal A uses the
// `bytes/4` heuristic for character-unit config values.

import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import {
  normalizeToolName,
  type SourceKind,
  type ToolResultEventRecord,
  type TurnRecord,
  type UserTurnRecord,
} from '@relayburn/reader';

import { lookupModelRate } from './cost.js';
import type { WasteAction, WasteFinding, WasteSeverity } from './findings.js';
import type { PricingTable } from './pricing.js';

// The knob the static-config check fires on. Claude's harness exposes this as
// an env var inside `.claude/settings.json` under `env.BASH_MAX_OUTPUT_LENGTH`.
// Above the threshold we flag — the CLI offers a one-line paste fix.
export const BASH_MAX_OUTPUT_ENV_KEY = 'BASH_MAX_OUTPUT_LENGTH';

// Default token threshold for both signals. 15k tokens of `tool_result`
// content rides in cache for every subsequent turn until compaction; above
// that the cache write+read tax dominates the call's actual usefulness.
export const DEFAULT_BLOAT_TOKEN_THRESHOLD = 15000;

// Inverse of bytesToTokens (kept in lockstep). Used by Signal A to surface
// a character-unit safe ceiling for `BASH_MAX_OUTPUT_LENGTH`, and by the
// finding adapter so the paste fix is in the unit `settings.json` speaks.
const BYTES_PER_TOKEN = 4;

// Minimum number of oversized events across the (source, toolName) bucket
// before we surface a finding. One stray oversized result is noise; a tool
// that repeatedly dumps massive output is a pattern.
const DEFAULT_MIN_OCCURRENCES = 1;

// Minimum number of (sized) events before we trust a p95 to bound the
// threshold. With fewer than this, the p95 collapses to an oversized event
// and would self-exclude the flag — so we fall back to the static 15k floor.
// 20 is a rule-of-thumb sample-size guard, picked deliberately small so a
// single-session debug ledger still surfaces oversized events.
const P95_SAMPLE_FLOOR = 20;

// bytes → tokens heuristic. Used by Signal A for character-unit config values.
// Matches the `bytes/4` heuristic used elsewhere in the codebase
// (`bytesToApproxTokens` in reader/userTurn.ts).
function bytesToTokens(bytes: number): number {
  if (bytes <= 0) return 0;
  return Math.ceil(bytes / BYTES_PER_TOKEN);
}

// Severity tiers shared with WasteFinding so a heterogeneous list ranks
// consistently. Mirrors the thresholds in findings.ts.
const SEVERITY_HIGH_USD = 0.5;
const SEVERITY_WARN_USD = 0.05;

function severityFromUsd(usd: number): WasteSeverity {
  if (usd >= SEVERITY_HIGH_USD) return 'high';
  if (usd >= SEVERITY_WARN_USD) return 'warn';
  return 'info';
}

export interface ToolOutputBloat {
  // Harness the finding applies to. Signal A is always 'claude-code' (the
  // only harness with a static knob); Signal B preserves whichever source
  // produced the oversized event.
  source: SourceKind;
  // 'static-config' for Signal A, 'observed-bloat' for Signal B.
  kind: 'static-config' | 'observed-bloat';
  // Tool name. For Signal A this is always 'Bash' (the knob targets it
  // exclusively). For Signal B it's the harness-native tool name (e.g. Claude
  // 'Bash', OpenCode 'bash', Codex 'shell', or any other tool).
  toolName: string;
  // Signal A only. The configured `BASH_MAX_OUTPUT_LENGTH` value when above
  // threshold, in characters (the unit `settings.json` speaks). Omitted on
  // Signal B.
  configuredLimit?: number;
  // Largest observed tool_result token count. For Signal A this is the
  // configured limit converted via `bytesToTokens` — the worst case the
  // harness will emit, in tokens. For Signal B it's the empirical max seen
  // across the bucket. Both branches expose this field in the same unit
  // (tokens) so consumers don't need to branch on `kind`.
  evidencedMaxOutput: number;
  // P95 of observed tool_result tokens across the bucket (Signal B only).
  // Useful for distinguishing "one fluke" from "this tool routinely dumps
  // huge output". Omitted on Signal A — there's no observed distribution.
  evidencedP95Output?: number;
  // Number of tool_result events that crossed the threshold (Signal B) or
  // 1 (Signal A — one config setting, one finding).
  occurrenceCount: number;
  // Approximate USD waste attributed to the bucket. For Signal B, this is
  // the sum across oversized events of `tokens × per-token input rate` of the
  // session's dominant model — i.e. the cost of carrying the oversized
  // payload as input on the very next turn. For Signal A it's 0 — the
  // configured-but-unused knob has no realized waste yet; an addressed fix
  // prevents future bloat.
  cost: number;
  // The session(s) where the oversized output was observed (Signal B). For
  // Signal A this lists the settings.json path that flagged. Useful for
  // surfacing actionable diagnostics in the WasteFinding adapter.
  evidence: string[];
}

// ---------------------------------------------------------------------------
// Signal A — static config check
// ---------------------------------------------------------------------------

// Claude `.claude/settings.json` shape we care about. The full schema is
// documented at https://docs.claude.com/en/docs/claude-code/settings; we only
// touch `env` here.
export interface ClaudeSettings {
  env?: Record<string, string>;
  [k: string]: unknown;
}

export interface LoadedClaudeSettings {
  // Path the file came from. Useful for the WasteFinding so the user knows
  // which file to edit.
  path: string;
  settings: ClaudeSettings;
}

// Resolve the user-level settings file. `~/.claude/settings.json` per Claude
// docs; honors the `HOME` env var so tests can inject an isolated home dir
// (matches the `homedir()` mocking pattern used elsewhere in the workspace).
export function userClaudeSettingsPath(): string {
  return path.join(homedir(), '.claude', 'settings.json');
}

// Resolve the project-level settings file relative to `cwd`. Project
// overrides user (matching Claude's actual settings precedence).
export function projectClaudeSettingsPath(cwd: string = process.cwd()): string {
  return path.join(cwd, '.claude', 'settings.json');
}

// Read and parse a `.claude/settings.json` from disk. Returns `undefined`
// when the file is missing or malformed — both cases mean "no setting to
// check", which is indistinguishable from "no waste". We deliberately do NOT
// throw on parse errors; the user's misconfigured settings.json should not
// crash `burn waste`.
export async function loadClaudeSettings(
  filePath: string,
): Promise<LoadedClaudeSettings | undefined> {
  let raw: string;
  try {
    raw = await readFile(filePath, 'utf8');
  } catch {
    return undefined;
  }
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return { path: filePath, settings: parsed as ClaudeSettings };
    }
  } catch {
    // Fall through.
  }
  return undefined;
}

export interface DetectStaticConfigBloatOptions {
  threshold?: number;
  // Pre-loaded settings files in precedence order (lowest → highest).
  // Project settings should appear AFTER user settings so the merge picks
  // them up as the override. Tests can pass synthetic settings without
  // touching disk.
  settings: LoadedClaudeSettings[];
}

// Inspect the merged Claude env block and emit a `ToolOutputBloat` when the
// `BASH_MAX_OUTPUT_LENGTH` knob is above the threshold. We only emit ONE
// finding per detector run even when both user and project files set the
// value — the offending value is the merged result, and the actionable file
// is the one that "won" the merge (project beats user).
export function detectStaticConfigBloat(
  opts: DetectStaticConfigBloatOptions,
): ToolOutputBloat[] {
  const threshold = opts.threshold ?? DEFAULT_BLOAT_TOKEN_THRESHOLD;
  // Merge env in precedence order. Higher-index wins (callers pass project
  // settings last per the contract).
  let mergedValue: string | undefined;
  let sourcePath: string | undefined;
  for (const loaded of opts.settings) {
    const env = loaded.settings.env;
    if (!env) continue;
    const v = env[BASH_MAX_OUTPUT_ENV_KEY];
    if (typeof v === 'string' && v.length > 0) {
      mergedValue = v;
      sourcePath = loaded.path;
    }
  }
  if (mergedValue === undefined || sourcePath === undefined) return [];
  const numericChars = parseInt(mergedValue, 10);
  if (!Number.isFinite(numericChars)) return [];
  // `BASH_MAX_OUTPUT_LENGTH` is a character count; the threshold is in
  // tokens. Convert before comparing — Signal B does the same conversion on
  // its `contentLength`-bytes input so the two signals share one threshold
  // semantics.
  const numericTokens = bytesToTokens(numericChars);
  if (numericTokens <= threshold) return [];
  return [
    {
      source: 'claude-code',
      kind: 'static-config',
      toolName: 'Bash',
      configuredLimit: numericChars,
      evidencedMaxOutput: numericTokens,
      occurrenceCount: 1,
      cost: 0,
      evidence: [sourcePath],
    },
  ];
}

// ---------------------------------------------------------------------------
// Signal B — observed bloat across sessions
// ---------------------------------------------------------------------------

export interface DetectObservedBloatOptions {
  // Cross-harness tool-result events.
  toolResultEvents: ToolResultEventRecord[];
  // User turn records with enriched content-sidecar data. Used to:
  //   1. Look up `toolName` for each event (joined by `tool_use_id`).
  //   2. Look up `approxTokens` from tool_result blocks (content-sidecar enrichment).
  //   3. Look up the model that consumed the next turn so we can price the
  //      oversized cache-input ride at the correct rate.
  // Tests can pass an empty array — the detector falls back to "Unknown"
  // tool names and a zero cost in that case (the bucket still emits).
  userTurns: UserTurnRecord[];
  turns: TurnRecord[];
  pricing: PricingTable;
  // Token threshold for "oversized". Defaults to
  // `max(DEFAULT_BLOAT_TOKEN_THRESHOLD, p95 of all observed tokens)`.
  // Override to pin a specific threshold (e.g. tests that want a low bar).
  threshold?: number;
  // Minimum number of oversized events in a (source, toolName) bucket before
  // we surface it. Default 1 — repeated oversized output is a pattern, but a
  // single instance is also genuine waste, just lower-severity.
  minOccurrences?: number;
}

interface ToolUseLookup {
  // `(source|sessionId|toolUseId)` -> tool name (post `normalizeToolName`).
  // We deliberately key on source+session so colliding tool_use_ids across
  // harnesses (unlikely but possible — Codex `call_*`, Claude UUIDs) don't
  // overwrite each other.
  toolNameByUseId: Map<string, string>;
  // `(source|sessionId|toolUseId)` -> approxTokens from user-turn blocks.
  // Populated from content-sidecar enrichment; falls back to 0 when missing.
  approxTokensByUseId: Map<string, number>;
  // `(source|sessionId|messageId)` -> model. Same dedupe rationale.
  modelByMessageId: Map<string, string>;
}

function buildLookup(userTurns: UserTurnRecord[], turns: TurnRecord[]): ToolUseLookup {
  const toolNameByUseId = new Map<string, string>();
  const approxTokensByUseId = new Map<string, number>();
  const modelByMessageId = new Map<string, string>();

  // Build approxTokens lookup from user-turn blocks (content-sidecar enrichment)
  for (const userTurn of userTurns) {
    for (const block of userTurn.blocks) {
      if (block.kind !== 'tool_result' || !block.toolUseId) continue;
      const key = `${userTurn.source}|${userTurn.sessionId}|${block.toolUseId}`;
      approxTokensByUseId.set(key, Math.max(0, block.approxTokens));
    }
  }

  // Build tool name and model lookups from turn records
  for (const t of turns) {
    modelByMessageId.set(`${t.source}|${t.sessionId}|${t.messageId}`, t.model);
    for (const call of t.toolCalls) {
      if (!call.id) continue;
      toolNameByUseId.set(`${t.source}|${t.sessionId}|${call.id}`, call.name);
    }
  }
  return { toolNameByUseId, approxTokensByUseId, modelByMessageId };
}

// p95 of an array of numbers. Empty array → 0.
function percentile(values: number[], p: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  // `nearest-rank`: idx = ceil(p/100 * N) - 1, clamped to [0, N-1]. For
  // small N this is the cleanest definition; matches numpy's "nearest" mode.
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[idx]!;
}

// Approximate cost of carrying `tokens` of input on the next turn at the
// session's dominant model rate. Returns 0 when the model isn't priced
// (foreign / synthetic models) — the bucket still emits, just with cost 0.
// `pricing[model].input` is per million tokens; we divide.
function priceCarryCost(tokens: number, model: string, pricing: PricingTable): number {
  const rate = lookupModelRate(model, pricing);
  if (!rate) return 0;
  return (tokens / 1_000_000) * rate.input;
}

export function detectObservedBloat(
  opts: DetectObservedBloatOptions,
): ToolOutputBloat[] {
  const events = opts.toolResultEvents;
  if (events.length === 0) return [];

  const lookup = buildLookup(opts.userTurns, opts.turns);
  const minOccurrences = opts.minOccurrences ?? DEFAULT_MIN_OCCURRENCES;

  // Compute the threshold: max(default, p95 across all events). The p95 only
  // contributes when we have enough samples for it to be meaningful — for
  // small slices (N < 20) the p95 collapses onto a single oversized event
  // and would self-exclude every flag. Below the guard we fall back to the
  // 15k floor only.
  const allTokens: number[] = [];
  for (const e of events) {
    const useKey = `${e.source}|${e.sessionId}|${e.toolUseId}`;
    let tokens = lookup.approxTokensByUseId.get(useKey);
    if (tokens === undefined && typeof e.contentLength === 'number' && e.contentLength > 0) {
      // Fallback to contentLength when enriched data is not available
      // (e.g., old fixtures without user-turn enrichment).
      tokens = bytesToTokens(e.contentLength);
    }
    if (tokens !== undefined && tokens > 0) allTokens.push(tokens);
  }
  const p95 = allTokens.length >= P95_SAMPLE_FLOOR ? percentile(allTokens, 95) : 0;
  const threshold = opts.threshold ?? Math.max(DEFAULT_BLOAT_TOKEN_THRESHOLD, p95);

  // Bucket oversized events by (source, normalizedToolName). Tools without a
  // matching `tool_calls` row (the join missed) bucket under "<unknown>" so
  // they're still surfaced — this is rare but happens when an event lands
  // before its parent turn (out-of-order parsing).
  interface Bucket {
    tokens: number[];
    sessions: Set<string>;
    cost: number;
  }
  const buckets = new Map<string, Bucket>();
  for (const e of events) {
    const useKey = `${e.source}|${e.sessionId}|${e.toolUseId}`;
    let tokens = lookup.approxTokensByUseId.get(useKey);
    if (tokens === undefined && typeof e.contentLength === 'number' && e.contentLength > 0) {
      // Fallback to contentLength when enriched data is not available
      tokens = bytesToTokens(e.contentLength);
    }
    if (tokens === undefined || tokens === 0 || tokens <= threshold) continue;
    const rawName = lookup.toolNameByUseId.get(useKey);
    const toolName = rawName ? normalizeToolName(rawName) : '<unknown>';
    const bucketKey = `${e.source}|${toolName}`;
    let bucket = buckets.get(bucketKey);
    if (!bucket) {
      bucket = { tokens: [], sessions: new Set(), cost: 0 };
      buckets.set(bucketKey, bucket);
    }
    bucket.tokens.push(tokens);
    bucket.sessions.add(e.sessionId);
    // Carry-cost: price the oversized tokens at the source turn's model
    // input rate. The "source turn" is the message that emitted the event,
    // looked up via messageId. When we can't find one (subagent / queue
    // event), we fall back to the first turn we have for that session.
    const messageKey = e.messageId
      ? `${e.source}|${e.sessionId}|${e.messageId}`
      : undefined;
    const model =
      (messageKey ? lookup.modelByMessageId.get(messageKey) : undefined) ??
      firstModelForSession(opts.turns, e.source, e.sessionId);
    if (model) {
      bucket.cost += priceCarryCost(tokens, model, opts.pricing);
    }
  }

  const out: ToolOutputBloat[] = [];
  for (const [key, bucket] of buckets) {
    if (bucket.tokens.length < minOccurrences) continue;
    const sep = key.indexOf('|');
    const source = key.slice(0, sep) as SourceKind;
    const toolName = key.slice(sep + 1);
    const max = Math.max(...bucket.tokens);
    const p95tokens = percentile(bucket.tokens, 95);
    out.push({
      source,
      kind: 'observed-bloat',
      toolName,
      evidencedMaxOutput: max,
      evidencedP95Output: p95tokens,
      occurrenceCount: bucket.tokens.length,
      cost: bucket.cost,
      evidence: [...bucket.sessions].sort(),
    });
  }
  // Sort by cost desc so the worst offender lands first.
  out.sort((a, b) => b.cost - a.cost);
  return out;
}

function firstModelForSession(
  turns: TurnRecord[],
  source: SourceKind,
  sessionId: string,
): string | undefined {
  for (const t of turns) {
    if (t.source === source && t.sessionId === sessionId) return t.model;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Top-level orchestration
// ---------------------------------------------------------------------------

export interface DetectToolOutputBloatOptions {
  // Signal A inputs.
  settings?: LoadedClaudeSettings[];
  // Signal B inputs.
  toolResultEvents?: ToolResultEventRecord[];
  userTurns?: UserTurnRecord[];
  turns?: TurnRecord[];
  pricing: PricingTable;
  // Shared tunables (apply to both signals).
  threshold?: number;
  minOccurrences?: number;
}

export function detectToolOutputBloat(
  opts: DetectToolOutputBloatOptions,
): ToolOutputBloat[] {
  const out: ToolOutputBloat[] = [];
  if (opts.settings && opts.settings.length > 0) {
    out.push(
      ...detectStaticConfigBloat({
        settings: opts.settings,
        ...(opts.threshold !== undefined ? { threshold: opts.threshold } : {}),
      }),
    );
  }
  if (opts.toolResultEvents && opts.toolResultEvents.length > 0) {
    out.push(
      ...detectObservedBloat({
        toolResultEvents: opts.toolResultEvents,
        userTurns: opts.userTurns ?? [],
        turns: opts.turns ?? [],
        pricing: opts.pricing,
        ...(opts.threshold !== undefined ? { threshold: opts.threshold } : {}),
        ...(opts.minOccurrences !== undefined ? { minOccurrences: opts.minOccurrences } : {}),
      }),
    );
  }
  return out;
}

// ---------------------------------------------------------------------------
// WasteFinding adapter (#56 envelope)
// ---------------------------------------------------------------------------

function fmtUsd(n: number): string {
  return `$${n.toFixed(4)}`;
}

// Adapt a `ToolOutputBloat` into the unified `WasteFinding` envelope so the
// CLI's `--findings` table can render it next to retry-loops, failure-runs,
// etc. Signal A and Signal B emit different action shapes:
//
//   - Signal A: a paste suggestion that drops the corrected env line into the
//     user's settings.json. The label names the file so the user knows
//     where to apply it.
//   - Signal B: an instruction-file paste suggesting `head` / `tail` / `grep`
//     filtering before reading. There's no single config knob that fixes
//     this on Codex / OpenCode — the agent has to learn to bound its own
//     output, and the user's CLAUDE.md / AGENTS.md is where that lesson
//     belongs.
export function toolOutputBloatToFinding(bloat: ToolOutputBloat): WasteFinding {
  // `sessionId` on the WasteFinding is the primary evidence ref. For Signal
  // A there's no session — substitute the settings.json path so the table
  // still has a meaningful identifier without inventing a synthetic id.
  const sessionId = bloat.kind === 'static-config' ? (bloat.evidence[0] ?? '') : (bloat.evidence[0] ?? '');

  if (bloat.kind === 'static-config') {
    // Paste suggestion is in characters — that's what `BASH_MAX_OUTPUT_LENGTH`
    // takes. Translate the token-unit threshold to chars via the same ratio
    // `bytesToTokens` uses so the suggestion sits exactly at the boundary.
    const safeChars = DEFAULT_BLOAT_TOKEN_THRESHOLD * BYTES_PER_TOKEN;
    const action: WasteAction = {
      type: 'paste',
      label: 'Reduce in settings.json',
      text: `"${BASH_MAX_OUTPUT_ENV_KEY}": "${safeChars}"`,
    };
    const configuredChars = bloat.configuredLimit;
    const configuredTokens = bloat.evidencedMaxOutput;
    return {
      kind: 'tool-output-bloat',
      severity: 'warn',
      sessionId,
      title:
        `${BASH_MAX_OUTPUT_ENV_KEY} configured at ${configuredChars?.toLocaleString() ?? '?'} chars ` +
        `(≈ ${configuredTokens.toLocaleString()} tokens, above ${DEFAULT_BLOAT_TOKEN_THRESHOLD.toLocaleString()})`,
      detail:
        `Claude is configured to allow Bash tool output up to ${configuredChars?.toLocaleString() ?? '?'} ` +
        `chars (≈ ${configuredTokens.toLocaleString()} tokens) per call. Above ` +
        `${DEFAULT_BLOAT_TOKEN_THRESHOLD.toLocaleString()} tokens (${safeChars.toLocaleString()} chars) the ` +
        `tool_result rides as cached input on every subsequent turn until compaction, dominating the ` +
        `call's actual usefulness. Source file: ${sessionId}.`,
      estimatedSavings: { tokensPerSession: configuredTokens },
      actions: [action],
    };
  }

  // Signal B
  const usdEstimate = bloat.cost;
  const severity = severityFromUsd(usdEstimate);
  const advice =
    `Avoid dumping full ${bloat.toolName} output into context. Filter first with head / tail / grep ` +
    `(or page through with sed -n) so only the relevant slice rides in cache on subsequent turns. ` +
    `Tool results > ${DEFAULT_BLOAT_TOKEN_THRESHOLD.toLocaleString()} tokens persist as cached input on every ` +
    `subsequent turn until compaction.`;
  const action: WasteAction = {
    type: 'paste',
    label: 'Add to CLAUDE.md / AGENTS.md',
    text:
      `When running ${bloat.toolName}, never dump full output into context. Filter first with ` +
      `\`head -n 200\`, \`tail -n 200\`, \`grep <pattern>\`, or paginate with \`sed -n '1,200p'\`. ` +
      `Each unfiltered tool_result above ${DEFAULT_BLOAT_TOKEN_THRESHOLD.toLocaleString()} tokens rides in cache on every ` +
      `subsequent turn until compaction.`,
  };
  return {
    kind: 'tool-output-bloat',
    severity,
    sessionId,
    title:
      `Oversized ${bloat.source} ${bloat.toolName} output: ${bloat.occurrenceCount}× ` +
      `(max ${bloat.evidencedMaxOutput.toLocaleString()} tok)`,
    detail:
      `${bloat.occurrenceCount} ${bloat.source} ${bloat.toolName} tool_result event(s) exceeded the ` +
      `${DEFAULT_BLOAT_TOKEN_THRESHOLD.toLocaleString()}-token threshold across ${bloat.evidence.length} ` +
      `session(s). Largest payload: ${bloat.evidencedMaxOutput.toLocaleString()} tokens. ` +
      (bloat.evidencedP95Output !== undefined
        ? `P95: ${bloat.evidencedP95Output.toLocaleString()} tokens. `
        : '') +
      `Estimated next-turn carry cost ${fmtUsd(usdEstimate)}. ${advice}`,
    estimatedSavings: { tokensPerSession: bloat.evidencedMaxOutput, usdPerSession: usdEstimate },
    actions: [action],
  };
}
