import type { SourceKind, ToolCall, TurnRecord } from '@relayburn/reader';

import { openArchive } from './archive.js';
import type { Query, EnrichedTurn } from './reader.js';
import type { Enrichment } from './schema.js';

/**
 * Read `EnrichedTurn[]` from the materialized archive (`archive.sqlite`)
 * instead of folding stamps over the JSONL ledger.
 *
 * Caller contract is identical to `queryAll(query)`: same filter shape, same
 * return shape — minus the per-turn fields the archive doesn't materialize
 * today (`filesTouched`, `sessionPath`, `subagent.description`, the
 * Edit/Write `editPreHash` / `editPostHash`). MCP tool consumers only read
 * `usage`, `model`, `sessionId`, and the enrichment columns, so those omissions
 * don't change the tool surface; analyses that need the missing fields should
 * keep using `queryAll`.
 *
 * Throws if `archive.sqlite` cannot be opened or the query fails. Callers that
 * want a transparent fallback (the MCP tool handlers do) should catch and
 * route to `queryAll`.
 *
 * The archive must already be built — see `buildArchive()`. This helper does
 * not trigger a build because the desirable cadence (cold-start vs.
 * per-call) is caller-policy.
 */
export async function queryTurnsFromArchive(q: Query = {}): Promise<EnrichedTurn[]> {
  const db = await openArchive();
  try {
    const where: string[] = [];
    const params: Array<string | number> = [];

    if (q.since) {
      where.push('ts >= ?');
      params.push(q.since);
    }
    if (q.until) {
      where.push('ts <= ?');
      params.push(q.until);
    }
    if (q.sessionId) {
      where.push('session_id = ?');
      params.push(q.sessionId);
    }
    if (q.source) {
      where.push('source = ?');
      params.push(q.source);
    }
    if (q.project) {
      // Match `turnPasses` semantics: project filter accepts either the raw
      // project path or the project key.
      where.push('(project = ? OR project_key = ?)');
      params.push(q.project, q.project);
    }

    const sql =
      `SELECT source, session_id, message_id, turn_index, ts, model, project, project_key,
              activity, stop_reason, has_edits, retries,
              is_sidechain, subagent_id, parent_subagent_id, parent_tool_use_id, subagent_type,
              input_tokens, output_tokens, reasoning_tokens,
              cache_read_tokens, cache_create_5m_tokens, cache_create_1h_tokens,
              workflow_id, agent_id, persona, tier, enrichment_json,
              attribution_fidelity, tokens_present, cost_present
         FROM turns` +
      (where.length > 0 ? ` WHERE ${where.join(' AND ')}` : '') +
      ` ORDER BY ts ASC, turn_index ASC`;

    const rows = db.prepare(sql).all(...params) as unknown as TurnRow[];

    // Pull tool_calls in a single batch keyed by (source, session_id,
    // message_id) — cheaper than N+1 selects when callers ask for an entire
    // session. We still select on the same WHERE so the working set matches.
    const toolCallRows = await loadToolCalls(db, where, params);
    const toolCallsByKey = new Map<string, ToolCall[]>();
    for (const row of toolCallRows) {
      const key = `${row.source}|${row.session_id}|${row.message_id}`;
      let list = toolCallsByKey.get(key);
      if (!list) {
        list = [];
        toolCallsByKey.set(key, list);
      }
      const tc: ToolCall = {
        id: row.tool_use_id ?? '',
        name: row.tool_name,
        argsHash: row.args_hash ?? '',
      };
      if (row.target !== null) tc.target = row.target;
      if (row.is_error !== null) tc.isError = Number(row.is_error) === 1;
      list.push(tc);
    }

    const out: EnrichedTurn[] = [];
    for (const row of rows) {
      const enrichment = parseEnrichment(row.enrichment_json);
      // Apply the enrichment-key filter against the parsed JSON. We could do
      // this in SQL with json_extract, but the working set has already been
      // narrowed by the indexed predicates above and stamp keys are an open
      // namespace, so an in-memory pass is simpler and equivalent.
      if (q.enrichment) {
        let drop = false;
        for (const [k, v] of Object.entries(q.enrichment)) {
          if (v === undefined) continue;
          if (enrichment[k] !== v) {
            drop = true;
            break;
          }
        }
        if (drop) continue;
      }
      const key = `${row.source}|${row.session_id}|${row.message_id}`;
      out.push(rowToEnrichedTurn(row, enrichment, toolCallsByKey.get(key) ?? []));
    }
    return out;
  } finally {
    db.close();
  }
}

interface TurnRow {
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
}

async function loadToolCalls(
  db: Awaited<ReturnType<typeof openArchive>>,
  where: string[],
  params: Array<string | number>,
): Promise<ToolCallRow[]> {
  // Map turn-table predicates onto the tool_calls join. The shared columns
  // (source, session_id, ts via the parent turn) are the only ones we filter
  // on, and `since` / `until` need a join through `turns` because tool_calls
  // doesn't carry a timestamp of its own.
  if (where.length === 0) {
    return db
      .prepare(
        'SELECT source, session_id, message_id, call_index, tool_use_id, tool_name, target, args_hash, is_error FROM tool_calls ORDER BY source, session_id, message_id, call_index',
      )
      .all() as unknown as ToolCallRow[];
  }
  const sql =
    `SELECT tc.source AS source, tc.session_id AS session_id, tc.message_id AS message_id,
            tc.call_index AS call_index, tc.tool_use_id AS tool_use_id, tc.tool_name AS tool_name,
            tc.target AS target, tc.args_hash AS args_hash, tc.is_error AS is_error
       FROM tool_calls tc
       JOIN turns t
         ON t.source = tc.source AND t.session_id = tc.session_id AND t.message_id = tc.message_id
      WHERE ${where.map((w) => qualifyTurnsClause(w)).join(' AND ')}
      ORDER BY tc.source, tc.session_id, tc.message_id, tc.call_index`;
  return db.prepare(sql).all(...params) as unknown as ToolCallRow[];
}

function qualifyTurnsClause(clause: string): string {
  // The WHERE clauses produced above all reference unqualified turn columns
  // (`ts`, `session_id`, `source`, `project`, `project_key`). The join above
  // aliases the turns table as `t`, so re-qualify here.
  return clause.replace(/\b(ts|session_id|source|project|project_key)\b/g, 't.$1');
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
    // fall through to empty
  }
  return {};
}

function rowToEnrichedTurn(
  row: TurnRow,
  enrichment: Enrichment,
  toolCalls: ToolCall[],
): EnrichedTurn {
  const turn: TurnRecord = {
    v: 1,
    source: row.source as SourceKind,
    sessionId: row.session_id,
    messageId: row.message_id,
    turnIndex: Number(row.turn_index),
    ts: row.ts,
    model: row.model,
    usage: {
      input: Number(row.input_tokens),
      output: Number(row.output_tokens),
      reasoning: Number(row.reasoning_tokens),
      cacheRead: Number(row.cache_read_tokens),
      cacheCreate5m: Number(row.cache_create_5m_tokens),
      cacheCreate1h: Number(row.cache_create_1h_tokens),
    },
    toolCalls,
  };
  if (row.project !== null) turn.project = row.project;
  if (row.project_key !== null) turn.projectKey = row.project_key;
  if (row.activity !== null) {
    turn.activity = row.activity as NonNullable<TurnRecord['activity']>;
  }
  if (row.stop_reason !== null) turn.stopReason = row.stop_reason;
  if (row.has_edits !== null) turn.hasEdits = Number(row.has_edits) === 1;
  if (row.retries !== null) turn.retries = Number(row.retries);
  if (
    row.is_sidechain !== null ||
    row.subagent_id !== null ||
    row.parent_subagent_id !== null ||
    row.parent_tool_use_id !== null ||
    row.subagent_type !== null
  ) {
    turn.subagent = {
      isSidechain: Number(row.is_sidechain ?? 0) === 1,
      ...(row.subagent_id !== null ? { agentId: row.subagent_id } : {}),
      ...(row.parent_subagent_id !== null ? { parentAgentId: row.parent_subagent_id } : {}),
      ...(row.parent_tool_use_id !== null ? { parentToolUseId: row.parent_tool_use_id } : {}),
      ...(row.subagent_type !== null ? { subagentType: row.subagent_type } : {}),
    };
  }
  return { ...turn, enrichment };
}
