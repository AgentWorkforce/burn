import { stat } from 'node:fs/promises';
import type { SQLInputValue } from 'node:sqlite';

import type {
  ActivityCategory,
  Coverage,
  Fidelity,
  FidelityClass,
  SourceKind,
  ToolCall,
  TurnRecord,
  UsageGranularity,
} from '@relayburn/reader';

import { openArchive } from './archive.js';
import { archivePath } from './paths.js';
import type { Query, EnrichedTurn } from './reader.js';
import type { Enrichment } from './schema.js';

/**
 * Read-side counterpart to `queryAll(query)` that issues SQL against the
 * derived archive (`~/.relayburn/archive.sqlite`) instead of streaming the
 * canonical `ledger.jsonl`. Returns the same `EnrichedTurn[]` shape so
 * downstream commands can swap implementations without touching their
 * aggregation code.
 *
 * Filters land as `WHERE` clauses so the b-tree indexes on `ts`, `model`,
 * `project_key`, and `workflow_id` (see `archive.ts`) carry the work.
 * Ordering matches `queryAll`: ledger insertion order, which is `(ts,
 * turn_index)` for sources that emit turns chronologically.
 *
 * Tool calls are hydrated from `tool_calls` keyed by `(source, session_id,
 * message_id)` so consumers that read `turn.toolCalls` (e.g. quality
 * detectors that count trailing failure streaks) keep working without an
 * extra round-trip.
 *
 * Fidelity is reconstructed from the persisted `attribution_fidelity` /
 * `tokens_present` / `cost_present` columns plus class-implied defaults for
 * the coverage shape — the archive intentionally does not store the full
 * `Fidelity` blob to keep the schema additive. NULL
 * `attribution_fidelity` maps back to `fidelity = undefined` so
 * `summarizeFidelity` buckets such turns under `unknown`, matching the
 * pre-migration JSON contract.
 *
 * Throws if the archive file is missing — the CLI is expected to call
 * `buildArchive()` (or `rebuildArchive()`) before this. Callers that want
 * the implicit fallback to `queryAll` should guard with
 * `archiveAvailable()`.
 */
export async function queryAllFromArchive(q: Query = {}): Promise<EnrichedTurn[]> {
  const db = await openArchive();
  try {
    const { sql, params } = buildSelect(q);
    const rows = db.prepare(sql).all(...params) as unknown as ArchiveTurnRow[];
    if (rows.length === 0) return [];

    // Bulk-hydrate tool calls keyed on (source, session_id, message_id).
    // SQLite's parameter limit is 999 by default in `node:sqlite`'s default
    // build; chunk to stay well below that. Each row contributes 3 params.
    const toolCallsByKey = await loadToolCallsForKeys(
      db,
      rows.map((r) => ({ source: r.source, sessionId: r.session_id, messageId: r.message_id })),
    );

    return rows.map((r) => rowToEnrichedTurn(r, toolCallsByKey));
  } finally {
    db.close();
  }
}

/**
 * Slimmer counterpart to `queryAllFromArchive`. Reads `EnrichedTurn[]`
 * directly from the materialized archive but skips the per-turn `fidelity`
 * synthesis — the only consumer (the MCP tool handlers) doesn't read
 * fidelity, so we save the bookkeeping. Callers that need fidelity should
 * use `queryAllFromArchive`. The archive must already be built — see
 * `buildArchive()` — and any open / query failure throws so callers can
 * route to `queryAll` for a transparent fallback.
 */
export async function queryTurnsFromArchive(q: Query = {}): Promise<EnrichedTurn[]> {
  const db = await openArchive();
  try {
    const { sql, params } = buildSelect(q);
    const rows = db.prepare(sql).all(...params) as unknown as ArchiveTurnRow[];
    if (rows.length === 0) return [];

    const toolCallsByKey = await loadToolCallsForKeys(
      db,
      rows.map((r) => ({ source: r.source, sessionId: r.session_id, messageId: r.message_id })),
    );

    return rows.map((r) => rowToEnrichedTurn(r, toolCallsByKey, { withFidelity: false }));
  } finally {
    db.close();
  }
}

export async function archiveAvailable(): Promise<boolean> {
  try {
    const s = await stat(archivePath());
    return s.isFile();
  } catch {
    return false;
  }
}

interface ArchiveTurnRow {
  source: string;
  session_id: string;
  message_id: string;
  turn_index: number | bigint;
  ts: string;
  model: string;
  project: string | null;
  project_key: string | null;
  activity: string | null;
  stop_reason: string | null;
  has_edits: number | bigint | null;
  retries: number | bigint | null;
  is_sidechain: number | bigint | null;
  subagent_id: string | null;
  parent_subagent_id: string | null;
  parent_tool_use_id: string | null;
  subagent_type: string | null;
  subagent_description: string | null;
  input_tokens: number | bigint;
  output_tokens: number | bigint;
  reasoning_tokens: number | bigint;
  cache_read_tokens: number | bigint;
  cache_create_5m_tokens: number | bigint;
  cache_create_1h_tokens: number | bigint;
  workflow_id: string | null;
  agent_id: string | null;
  persona: string | null;
  tier: string | null;
  enrichment_json: string | null;
  attribution_fidelity: string | null;
  tokens_present: number | bigint | null;
  cost_present: number | bigint | null;
}

interface ToolCallRow {
  source: string;
  session_id: string;
  message_id: string;
  call_index: number | bigint;
  tool_use_id: string | null;
  tool_name: string;
  target: string | null;
  args_hash: string | null;
  is_error: number | bigint | null;
  replaced_tools: string | null;
  collapsed_calls: number | bigint | null;
}

function buildSelect(q: Query): { sql: string; params: SQLInputValue[] } {
  const wheres: string[] = [];
  const params: SQLInputValue[] = [];
  if (q.since) {
    wheres.push('ts >= ?');
    params.push(q.since);
  }
  if (q.until) {
    wheres.push('ts <= ?');
    params.push(q.until);
  }
  if (q.project) {
    // Preserve `queryAll` semantics: a turn matches when either `project`
    // or `projectKey` equals the filter value. Both columns are indexed
    // (well, project_key is — `project` falls back to a scan, same as the
    // ledger walk).
    wheres.push('(project = ? OR project_key = ?)');
    params.push(q.project, q.project);
  }
  if (q.sessionId) {
    wheres.push('session_id = ?');
    params.push(q.sessionId);
  }
  if (q.source) {
    wheres.push('source = ?');
    params.push(q.source);
  }
  if (q.enrichment) {
    for (const [k, v] of Object.entries(q.enrichment)) {
      if (v === undefined) continue;
      const col = ENRICHMENT_COLUMN[k];
      if (col) {
        // Materialized enrichment column — index-friendly equality.
        wheres.push(`${col} = ?`);
        params.push(v);
      } else {
        // Fall back to the JSON blob for keys we didn't promote to a
        // column. Slow path (full scan) but matches the queryAll contract
        // for arbitrary stamp keys.
        wheres.push(`json_extract(enrichment_json, '$.' || ?) = ?`);
        params.push(k, v);
      }
    }
  }

  // Order matches `queryAll`'s emission order from streaming the ledger
  // (turns emit roughly in (ts, turn_index) order; preserving that keeps
  // downstream `sort` / `[turns.length-1]` lookups stable across paths).
  const sql = [
    'SELECT * FROM turns',
    wheres.length > 0 ? `WHERE ${wheres.join(' AND ')}` : '',
    'ORDER BY ts, turn_index',
  ]
    .filter(Boolean)
    .join(' ');
  return { sql, params };
}

const ENRICHMENT_COLUMN: Record<string, string | undefined> = {
  workflowId: 'workflow_id',
  agentId: 'agent_id',
  persona: 'persona',
  tier: 'tier',
};

async function loadToolCallsForKeys(
  db: Awaited<ReturnType<typeof openArchive>>,
  keys: Array<{ source: string; sessionId: string; messageId: string }>,
): Promise<Map<string, ToolCall[]>> {
  const out = new Map<string, ToolCall[]>();
  if (keys.length === 0) return out;
  // Dedup keys — multiple turn rows can never share (source, session_id,
  // message_id) (PK guarantees), so this is just defensive.
  const seen = new Set<string>();
  const distinct: Array<{ source: string; sessionId: string; messageId: string }> = [];
  for (const k of keys) {
    const id = `${k.source}|${k.sessionId}|${k.messageId}`;
    if (seen.has(id)) continue;
    seen.add(id);
    distinct.push(k);
  }
  // SQLite default SQLITE_MAX_VARIABLE_NUMBER is 32766 in modern builds, but
  // node:sqlite ships with a more conservative default. 250 keys × 3 params
  // = 750 placeholders per chunk leaves plenty of headroom.
  const CHUNK = 250;
  for (let i = 0; i < distinct.length; i += CHUNK) {
    const chunk = distinct.slice(i, i + CHUNK);
    const placeholders = chunk.map(() => '(?, ?, ?)').join(', ');
    const params: SQLInputValue[] = [];
    for (const k of chunk) {
      params.push(k.source, k.sessionId, k.messageId);
    }
    const sql = `
      SELECT source, session_id, message_id, call_index, tool_use_id,
             tool_name, target, args_hash, is_error,
             replaced_tools, collapsed_calls
      FROM tool_calls
      WHERE (source, session_id, message_id) IN (${placeholders})
      ORDER BY source, session_id, message_id, call_index
    `;
    const rows = db.prepare(sql).all(...params) as unknown as ToolCallRow[];
    for (const row of rows) {
      const key = `${row.source}|${row.session_id}|${row.message_id}`;
      let list = out.get(key);
      if (!list) {
        list = [];
        out.set(key, list);
      }
      const tc: ToolCall = {
        // Persisted `tool_use_id` is the only thing the writer sees on the
        // ledger as `ToolCall.id`; older rows that lacked one were stored
        // as NULL — surface as empty string so the type stays satisfied.
        id: row.tool_use_id ?? '',
        name: row.tool_name,
        argsHash: row.args_hash ?? '',
      };
      if (row.target !== null) tc.target = row.target;
      if (row.is_error !== null) tc.isError = Number(row.is_error) === 1;
      if (row.replaced_tools !== null) {
        try {
          const parsed = JSON.parse(row.replaced_tools) as unknown;
          if (Array.isArray(parsed)) {
            const names = parsed.filter(
              (v): v is string => typeof v === 'string' && v.length > 0,
            );
            if (names.length > 0) tc.replacedTools = names;
          }
        } catch {
          // Malformed JSON in an additive column — drop silently rather than
          // failing the whole archive read.
        }
      }
      if (row.collapsed_calls !== null) {
        const collapsed = Number(row.collapsed_calls);
        if (Number.isFinite(collapsed) && collapsed > 0) tc.collapsedCalls = collapsed;
      }
      list.push(tc);
    }
  }
  return out;
}

function rowToEnrichedTurn(
  r: ArchiveTurnRow,
  toolCallsByKey: Map<string, ToolCall[]>,
  opts: { withFidelity?: boolean } = { withFidelity: true },
): EnrichedTurn {
  const enrichment = parseEnrichment(r.enrichment_json);
  const toolCalls = toolCallsByKey.get(`${r.source}|${r.session_id}|${r.message_id}`) ?? [];
  const turn: TurnRecord = {
    v: 1,
    source: r.source as SourceKind,
    sessionId: r.session_id,
    messageId: r.message_id,
    turnIndex: Number(r.turn_index),
    ts: r.ts,
    model: r.model,
    usage: {
      input: Number(r.input_tokens),
      output: Number(r.output_tokens),
      reasoning: Number(r.reasoning_tokens),
      cacheRead: Number(r.cache_read_tokens),
      cacheCreate5m: Number(r.cache_create_5m_tokens),
      cacheCreate1h: Number(r.cache_create_1h_tokens),
    },
    toolCalls,
  };
  if (r.project !== null) turn.project = r.project;
  if (r.project_key !== null) turn.projectKey = r.project_key;
  if (r.activity !== null) turn.activity = r.activity as ActivityCategory;
  if (r.stop_reason !== null) turn.stopReason = r.stop_reason;
  if (r.has_edits !== null) turn.hasEdits = Number(r.has_edits) === 1;
  if (r.retries !== null) turn.retries = Number(r.retries);
  // Subagent block: only emit when at least one of its fields is populated,
  // matching the on-ledger shape (a Codex turn with no sidechain doesn't
  // carry an empty `subagent` object; `is_sidechain` lands NULL → skip).
  if (
    r.is_sidechain !== null ||
    r.subagent_id !== null ||
    r.parent_subagent_id !== null ||
    r.parent_tool_use_id !== null ||
    r.subagent_type !== null ||
    r.subagent_description !== null
  ) {
    const subagent: NonNullable<TurnRecord['subagent']> = {
      isSidechain: Number(r.is_sidechain ?? 0) === 1,
    };
    if (r.subagent_id !== null) subagent.agentId = r.subagent_id;
    if (r.parent_subagent_id !== null) subagent.parentAgentId = r.parent_subagent_id;
    if (r.parent_tool_use_id !== null) subagent.parentToolUseId = r.parent_tool_use_id;
    if (r.subagent_type !== null) subagent.subagentType = r.subagent_type;
    if (r.subagent_description !== null) subagent.description = r.subagent_description;
    turn.subagent = subagent;
  }
  if (opts.withFidelity !== false) {
    const fidelity = synthesizeFidelity(r);
    if (fidelity) turn.fidelity = fidelity;
  }
  return { ...turn, enrichment };
}

function parseEnrichment(json: string | null): Enrichment {
  if (!json) return {};
  try {
    const parsed = JSON.parse(json) as unknown;
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      const out: Enrichment = {};
      for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
        if (typeof v === 'string') out[k] = v;
      }
      return out;
    }
  } catch {
    // Fall through to empty enrichment — corrupted JSON shouldn't crash
    // the read path.
  }
  return {};
}

/**
 * Reconstruct a `Fidelity` from the projected archive columns.
 *
 * We only persist `class`, `tokens_present`, and `cost_present`; the
 * full `coverage` and `granularity` shape isn't stored to keep the schema
 * additive. Synthesize a coverage object that matches the class semantics so
 * `summarizeFidelity` buckets the turn into the same `byClass` slot the
 * pre-migration `queryAll` would have produced. `byGranularity` and
 * `missingCoverage` will reflect the synthesized shape — close to but not
 * always byte-identical with the original `Fidelity` if the source
 * (Codex/OpenCode) populated coverage flags differently. The class bucket
 * IS byte-identical, which is what the text-mode fidelity notice and the
 * `unknown`/`full`/`partial`/etc. counts in JSON depend on.
 */
function synthesizeFidelity(r: ArchiveTurnRow): Fidelity | undefined {
  if (r.attribution_fidelity === null) return undefined;
  const cls = r.attribution_fidelity as FidelityClass;
  const tokensPresent = r.tokens_present !== null && Number(r.tokens_present) === 1;
  const costPresent = r.cost_present !== null && Number(r.cost_present) === 1;
  const granularity: UsageGranularity = costPresent ? 'cost-only' : 'per-turn';
  const coverage = coverageForClass(cls, tokensPresent);
  return { class: cls, granularity, coverage };
}

function coverageForClass(cls: FidelityClass, tokensPresent: boolean): Coverage {
  // Sensible per-class defaults. `full` → everything true; `cost-only` →
  // nothing true; `usage-only` → tokens true / structural false; `partial`
  // / `aggregate-only` → split the difference using `tokensPresent`.
  switch (cls) {
    case 'full':
      return {
        hasInputTokens: true,
        hasOutputTokens: true,
        hasReasoningTokens: true,
        hasCacheReadTokens: true,
        hasCacheCreateTokens: true,
        hasToolCalls: true,
        hasToolResultEvents: true,
        hasSessionRelationships: true,
        hasRawContent: true,
      };
    case 'usage-only':
      return {
        hasInputTokens: tokensPresent,
        hasOutputTokens: tokensPresent,
        hasReasoningTokens: false,
        hasCacheReadTokens: tokensPresent,
        hasCacheCreateTokens: tokensPresent,
        hasToolCalls: false,
        hasToolResultEvents: false,
        hasSessionRelationships: false,
        hasRawContent: false,
      };
    case 'partial':
      return {
        hasInputTokens: tokensPresent,
        hasOutputTokens: tokensPresent,
        hasReasoningTokens: false,
        hasCacheReadTokens: false,
        hasCacheCreateTokens: false,
        hasToolCalls: false,
        hasToolResultEvents: false,
        hasSessionRelationships: false,
        hasRawContent: false,
      };
    case 'aggregate-only':
      return {
        hasInputTokens: false,
        hasOutputTokens: false,
        hasReasoningTokens: false,
        hasCacheReadTokens: false,
        hasCacheCreateTokens: false,
        hasToolCalls: false,
        hasToolResultEvents: false,
        hasSessionRelationships: false,
        hasRawContent: false,
      };
    case 'cost-only':
    default:
      return {
        hasInputTokens: false,
        hasOutputTokens: false,
        hasReasoningTokens: false,
        hasCacheReadTokens: false,
        hasCacheCreateTokens: false,
        hasToolCalls: false,
        hasToolResultEvents: false,
        hasSessionRelationships: false,
        hasRawContent: false,
      };
  }
}
