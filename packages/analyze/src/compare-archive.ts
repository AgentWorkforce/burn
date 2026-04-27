import { openArchive, type Query } from '@relayburn/ledger';

import {
  DEFAULT_MIN_SAMPLE,
  type CompareCell,
  type CompareOptions,
  type CompareTable,
} from './compare.js';
import { costForUsage, lookupModelRate } from './cost.js';

export interface CompareFromArchiveResult {
  table: CompareTable;
  /**
   * Total turn count (pre-`--models` filter) matching `q`, used to populate
   * the "turns analyzed" header line in text mode and the `analyzedTurns`
   * field in `--json`. Mirrors `(await queryAll(q)).length` from the
   * legacy path.
   */
  analyzedTurns: number;
}

/**
 * Build a `CompareTable` from the analytics archive (`archive.sqlite`)
 * instead of streaming the full ledger. Issued as a single grouped SQL
 * query over `turns` plus a tiny per-(model, activity) follow-up for the
 * median-retries quantile, and one top-level `COUNT(*)` for the
 * `analyzedTurns` header. See issue #88.
 *
 * Behavior is byte-identical to `buildCompareTable(await queryAll(q), opts)`
 * for the fixtures the parity tests cover. Cost math is computed in JS over
 * SQL-aggregated token sums (linear, so equivalent to per-turn summation),
 * with the source-specific reasoning-mode override (Codex's
 * `included_in_output`) preserved by grouping on `source` alongside
 * (model, activity) and folding into cells afterwards.
 *
 * Falling back to the legacy in-memory path is the caller's responsibility:
 * see `runCompare` in `@relayburn/cli` for the `--no-archive` /
 * `RELAYBURN_ARCHIVE=0` switch.
 */
export async function compareFromArchive(
  q: Query,
  opts: CompareOptions,
): Promise<CompareFromArchiveResult> {
  const minSample = opts.minSample ?? DEFAULT_MIN_SAMPLE;
  const modelFilter = opts.models && opts.models.length > 0 ? new Set(opts.models) : null;

  const db = await openArchive();
  try {
    const where = buildWhere(q);
    // Group on (model, activity, source). Source is included so we can apply
    // the per-source reasoning-mode override (Codex bills reasoning inside
    // output) without losing per-turn fidelity. Within a single model cell,
    // pricedTurns either equals turns (model has a rate) or 0 (no rate),
    // because `lookupModelRate` is a function of model only.
    const sql = `
      SELECT
        COALESCE(NULLIF(model, ''), 'unknown')             AS model,
        COALESCE(activity, 'unclassified')                 AS activity,
        source                                             AS source,
        COUNT(*)                                           AS turns,
        SUM(CASE WHEN has_edits = 1 THEN 1 ELSE 0 END)     AS edit_turns,
        SUM(CASE WHEN has_edits = 1 AND COALESCE(retries, 0) = 0
                 THEN 1 ELSE 0 END)                        AS one_shot_turns,
        SUM(input_tokens)                                  AS input_tokens,
        SUM(output_tokens)                                 AS output_tokens,
        SUM(reasoning_tokens)                              AS reasoning_tokens,
        SUM(cache_read_tokens)                             AS cache_read_tokens,
        SUM(cache_create_5m_tokens)                        AS cache_create_5m_tokens,
        SUM(cache_create_1h_tokens)                        AS cache_create_1h_tokens
      FROM turns
      ${where.sql}
      GROUP BY COALESCE(NULLIF(model, ''), 'unknown'),
               COALESCE(activity, 'unclassified'),
               source
    `;
    const rows = db.prepare(sql).all(...where.params) as Array<{
      model: string;
      activity: string;
      source: string;
      turns: number | bigint;
      edit_turns: number | bigint;
      one_shot_turns: number | bigint;
      input_tokens: number | bigint;
      output_tokens: number | bigint;
      reasoning_tokens: number | bigint;
      cache_read_tokens: number | bigint;
      cache_create_5m_tokens: number | bigint;
      cache_create_1h_tokens: number | bigint;
    }>;

    const byModelCategory = new Map<string, Map<string, CellAccum>>();
    const modelTotals = new Map<string, { turns: number; totalCost: number }>();
    const modelSet = new Set<string>();
    const categorySet = new Set<string>();

    // Pre-seed modelSet from the --models filter so a model the user
    // explicitly asked about stays visible (as an all-empty column with
    // coverage notes) even if zero turns matched. Mirrors the in-memory
    // `buildCompareTable` behavior.
    if (modelFilter) {
      for (const m of modelFilter) {
        modelSet.add(m);
        modelTotals.set(m, { turns: 0, totalCost: 0 });
      }
    }

    for (const r of rows) {
      const model = r.model;
      if (modelFilter && !modelFilter.has(model)) continue;
      const cat = r.activity;
      modelSet.add(model);
      categorySet.add(cat);

      let byCat = byModelCategory.get(model);
      if (!byCat) {
        byCat = new Map();
        byModelCategory.set(model, byCat);
      }
      let acc = byCat.get(cat);
      if (!acc) {
        acc = newAccum();
        byCat.set(cat, acc);
      }

      const turns = Number(r.turns);
      const editTurns = Number(r.edit_turns);
      const oneShotTurns = Number(r.one_shot_turns);
      const inputTokens = Number(r.input_tokens);
      const outputTokens = Number(r.output_tokens);
      const reasoningTokens = Number(r.reasoning_tokens);
      const cacheReadTokens = Number(r.cache_read_tokens);
      const cacheCreate5mTokens = Number(r.cache_create_5m_tokens);
      const cacheCreate1hTokens = Number(r.cache_create_1h_tokens);

      acc.turns += turns;
      acc.editTurns += editTurns;
      acc.oneShotTurns += oneShotTurns;
      acc.cacheRead += cacheReadTokens;
      acc.tokenDenominator +=
        inputTokens + cacheReadTokens + cacheCreate5mTokens + cacheCreate1hTokens;

      const mt = modelTotals.get(model) ?? { turns: 0, totalCost: 0 };
      mt.turns += turns;

      // Cost: applied per (source, model) group. lookupModelRate is a
      // function of model only, so within a model cell pricedTurns is
      // either `turns_in_group` (model priced) or 0 (model unpriced).
      const rate = lookupModelRate(model, opts.pricing);
      if (rate) {
        const reasoningMode = r.source === 'codex' ? 'included_in_output' : undefined;
        const breakdown = costForUsage(
          {
            input: inputTokens,
            output: outputTokens,
            reasoning: reasoningTokens,
            cacheRead: cacheReadTokens,
            cacheCreate5m: cacheCreate5mTokens,
            cacheCreate1h: cacheCreate1hTokens,
          },
          model,
          opts.pricing,
          reasoningMode ? { reasoningMode } : {},
        );
        if (breakdown) {
          acc.pricedTurns += turns;
          acc.totalCost += breakdown.total;
          mt.totalCost += breakdown.total;
        }
      }
      modelTotals.set(model, mt);
    }

    // Per-cell median-retries follow-up. Only run for cells with editTurns > 0
    // — empty edit cells return medianRetries: null and need no follow-up.
    // Each query is keyed on (source, session_id) indexes only at the WHERE
    // level via (model, activity), but the planner uses idx_turns_model and
    // idx_turns_activity, which is the documented use case for those
    // indexes. For a typical cell this returns at most a few hundred rows.
    const medianStmt = db.prepare(
      `SELECT retries
       FROM turns
       ${where.sql ? where.sql + ' AND ' : 'WHERE '}
         COALESCE(NULLIF(model, ''), 'unknown') = ?
         AND COALESCE(activity, 'unclassified') = ?
         AND has_edits = 1`,
    );
    const cells: CompareTable['cells'] = {};
    const models = [...modelSet].sort((a, b) => {
      const ca = modelTotals.get(a)?.totalCost ?? 0;
      const cb = modelTotals.get(b)?.totalCost ?? 0;
      if (cb !== ca) return cb - ca;
      return a.localeCompare(b);
    });
    const categories = [...categorySet].sort((a, b) => {
      let ta = 0;
      let tb = 0;
      for (const m of models) {
        ta += byModelCategory.get(m)?.get(a)?.turns ?? 0;
        tb += byModelCategory.get(m)?.get(b)?.turns ?? 0;
      }
      if (tb !== ta) return tb - ta;
      return a.localeCompare(b);
    });
    for (const m of models) {
      cells[m] = {};
      for (const cat of categories) {
        const acc = byModelCategory.get(m)?.get(cat);
        let medianRetries: number | null = null;
        if (acc && acc.editTurns > 0) {
          const retryRows = medianStmt.all(
            ...where.params,
            m,
            cat,
          ) as Array<{ retries: number | bigint | null }>;
          const samples: number[] = [];
          for (const rr of retryRows) {
            samples.push(Number(rr.retries ?? 0));
          }
          medianRetries = median(samples);
        }
        cells[m]![cat] = toCell(acc, minSample, medianRetries);
      }
    }

    const totals: CompareTable['totals'] = {};
    for (const [m, v] of modelTotals) totals[m] = v;

    // analyzedTurns is the pre-`--models` count over the same WHERE clause.
    // Pre-models because the legacy path computes it as `turns.length` from
    // queryAll(q), which doesn't see opts.models.
    const countRow = db
      .prepare(`SELECT COUNT(*) AS n FROM turns ${where.sql}`)
      .get(...where.params) as { n: number | bigint };
    const analyzedTurns = Number(countRow.n);

    return {
      table: { models, categories, cells, totals, minSample },
      analyzedTurns,
    };
  } finally {
    db.close();
  }
}

interface BuiltWhere {
  sql: string;
  params: Array<string>;
}

function buildWhere(q: Query): BuiltWhere {
  const conditions: string[] = [];
  const params: string[] = [];
  if (q.since) {
    conditions.push('ts >= ?');
    params.push(q.since);
  }
  if (q.until) {
    conditions.push('ts <= ?');
    params.push(q.until);
  }
  if (q.project) {
    // Mirror the in-memory Query semantics: a project filter matches either
    // the literal `project` path or the git-canonical `project_key`.
    conditions.push('(project = ? OR project_key = ?)');
    params.push(q.project, q.project);
  }
  if (q.sessionId) {
    conditions.push('session_id = ?');
    params.push(q.sessionId);
  }
  if (q.source) {
    conditions.push('source = ?');
    params.push(q.source);
  }
  if (q.enrichment) {
    for (const [k, v] of Object.entries(q.enrichment)) {
      if (v === undefined) continue;
      const col = ENRICHMENT_COLUMN[k];
      if (!col) {
        // Unknown enrichment key — fall back to JSON_EXTRACT on the
        // materialized `enrichment_json` column so callers keep working
        // when they query a non-canonical key (matches the in-memory
        // path's "any key in enrichment" semantics).
        conditions.push(`COALESCE(json_extract(enrichment_json, ?), '') = ?`);
        params.push(`$.${k}`, v);
        continue;
      }
      conditions.push(`${col} = ?`);
      params.push(v);
    }
  }
  return {
    sql: conditions.length === 0 ? '' : `WHERE ${conditions.join(' AND ')}`,
    params,
  };
}

// Canonical enrichment keys that have dedicated columns in the archive.
// Anything else falls through to a json_extract on `enrichment_json`.
const ENRICHMENT_COLUMN: Record<string, string> = {
  workflowId: 'workflow_id',
  agentId: 'agent_id',
  persona: 'persona',
  tier: 'tier',
};

interface CellAccum {
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  pricedTurns: number;
  totalCost: number;
  cacheRead: number;
  tokenDenominator: number;
}

function newAccum(): CellAccum {
  return {
    turns: 0,
    editTurns: 0,
    oneShotTurns: 0,
    pricedTurns: 0,
    totalCost: 0,
    cacheRead: 0,
    tokenDenominator: 0,
  };
}

function toCell(
  acc: CellAccum | undefined,
  minSample: number,
  medianRetries: number | null,
): CompareCell {
  if (!acc || acc.turns === 0) {
    return {
      turns: 0,
      editTurns: 0,
      oneShotTurns: 0,
      pricedTurns: 0,
      totalCost: 0,
      costPerTurn: null,
      oneShotRate: null,
      cacheHitRate: null,
      medianRetries: null,
      noData: true,
      insufficientSample: false,
    };
  }
  return {
    turns: acc.turns,
    editTurns: acc.editTurns,
    oneShotTurns: acc.oneShotTurns,
    pricedTurns: acc.pricedTurns,
    totalCost: acc.totalCost,
    costPerTurn: acc.pricedTurns > 0 ? acc.totalCost / acc.pricedTurns : null,
    oneShotRate: acc.editTurns > 0 ? acc.oneShotTurns / acc.editTurns : null,
    cacheHitRate: acc.tokenDenominator > 0 ? acc.cacheRead / acc.tokenDenominator : null,
    medianRetries: acc.editTurns > 0 ? medianRetries : null,
    noData: false,
    insufficientSample: acc.turns < minSample,
  };
}

function median(xs: number[]): number {
  if (xs.length === 0) return 0;
  const s = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 0 ? (s[mid - 1]! + s[mid]!) / 2 : s[mid]!;
}

