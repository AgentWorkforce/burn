import {
  buildArchive,
  queryAll,
  queryAllFromArchive,
  queryTurnsFromArchive,
  queryUserTurns,
  queryToolResultEvents,
} from '@relayburn/ledger';
import {
  aggregateByBash,
  aggregateByBashVerb,
  aggregateByFile,
  aggregateBySubagent,
  attributeOverhead,
  buildGhostSurfaceInputs,
  buildTrimRecommendations,
  costForTurn,
  detectGhostSurface,
  detectPatterns,
  detectToolCallPatterns,
  detectToolOutputBloat,
  findingsFromPatterns,
  findOverheadFiles,
  ghostSurfaceToFinding,
  loadClaudeSettings,
  loadOverheadFile,
  loadPricing,
  projectClaudeSettingsPath,
  renderUnifiedDiffForRecommendation,
  summarizeFidelity,
  sumCosts,
  attributeHotspots,
  toolCallPatternToFinding,
  toolOutputBloatToFinding,
  userClaudeSettingsPath,
} from '@relayburn/analyze';
import { ingestAll } from '@relayburn/ingest';
import { parseBashCommand, resolveProject } from '@relayburn/reader';
import { readFile } from 'node:fs/promises';
import * as path from 'node:path';

function withHome(home, fn) {
  const prev = process.env.RELAYBURN_HOME;
  if (home) process.env.RELAYBURN_HOME = home;
  return Promise.resolve(fn()).finally(() => {
    if (home) {
      if (prev === undefined) delete process.env.RELAYBURN_HOME;
      else process.env.RELAYBURN_HOME = prev;
    }
  });
}

// Bring the SQLite archive current and query against it, falling back to a
// full ledger walk if the archive can't be built or read. Mirrors the strategy
// the CLI's loadTurns() uses so SDK consumers (and the MCP server, which now
// calls through here) get the same hot-path performance without re-implementing
// the fallback logic in every caller. `onLog` lets callers surface the
// fallback reason; defaults to a no-op so library use stays quiet.
async function loadTurnsViaArchive(q, onLog) {
  try {
    await buildArchive();
    return await queryAllFromArchive(q);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    onLog?.(`archive query failed, falling back to ledger walk: ${msg}`);
    return queryAll(q);
  }
}

async function loadSessionTurnsViaArchive(sessionId, onLog) {
  try {
    await buildArchive();
    return await queryTurnsFromArchive({ sessionId });
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    onLog?.(`archive query failed, falling back to ledger walk: ${msg}`);
    return queryAll({ sessionId });
  }
}

// Accept either a CLI-style relative range (`24h`, `7d`, `4w`, `2m`) or an
// ISO timestamp and return an ISO string the ledger query can compare. The
// ledger filter does lexical string comparison on `turn.ts`, so passing a raw
// `7d` would silently filter every turn out (since `'7'` > `'2'` lexically).
// Lifted from `packages/cli/src/format.ts` so direct SDK callers (and future
// MCP tools) get the same forgiving input shape the CLI users see, without
// the silent-drop trap.
function normalizeSince(since) {
  if (since === undefined) return undefined;
  if (typeof since !== 'string' || since.length === 0) return undefined;
  const m = /^(\d+)([hdwm])$/.exec(since);
  if (!m) {
    const d = new Date(since);
    if (Number.isNaN(d.getTime())) {
      throw new Error(`invalid since: ${since} (expected ISO timestamp or relative range like 7d)`);
    }
    return d.toISOString();
  }
  const n = parseInt(m[1], 10);
  const unit = m[2];
  const ms =
    unit === 'h'
      ? n * 3600_000
      : unit === 'd'
        ? n * 86400_000
        : unit === 'w'
          ? n * 7 * 86400_000
          : /* m */ n * 30 * 86400_000;
  return new Date(Date.now() - ms).toISOString();
}

export class Ledger {
  static async open(opts = {}) {
    return new Ledger(opts.home);
  }

  constructor(home) {
    this.home = home;
  }
}

export async function ingest(opts = {}) {
  return withHome(opts.ledgerHome, async () => ingestAll());
}

export async function summary(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const q = { sessionId: opts.session, project: opts.project, since: normalizeSince(opts.since) };
    const turns = await loadTurnsViaArchive(q, opts.onLog);
    const pricing = await loadPricing();
    const byTool = new Map();
    const byModel = new Map();
    let totalTokens = 0;
    let totalCost = 0;

    for (const t of turns) {
      const c = costForTurn(t, pricing)?.total ?? 0;
      const usage =
        t.usage.input +
        t.usage.output +
        t.usage.reasoning +
        t.usage.cacheRead +
        t.usage.cacheCreate5m +
        t.usage.cacheCreate1h;
      totalTokens += usage;
      totalCost += c;

      const model = byModel.get(t.model) ?? { model: t.model, tokens: 0, cost: 0 };
      model.tokens += usage;
      model.cost += c;
      byModel.set(t.model, model);

      for (const call of t.toolCalls) {
        const tool = byTool.get(call.name) ?? { tool: call.name, tokens: 0, cost: 0, count: 0 };
        tool.tokens += usage;
        tool.cost += c;
        tool.count += 1;
        byTool.set(call.name, tool);
      }
    }

    return {
      totalTokens,
      totalCost,
      turnCount: turns.length,
      byTool: [...byTool.values()],
      byModel: [...byModel.values()],
    };
  });
}

// Compact session-scoped cost summary. Same numbers as `summary({ session })`
// but shaped for callers that just want the headline: totalUSD, totalTokens,
// turnCount, distinct models. The MCP `burn__sessionCost` tool wraps this
// directly so the cost shape lives in one place. `note` is set when the
// session is empty or when no session id was provided so MCP clients can
// surface a human-readable reason without re-deriving it.
export async function sessionCost(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const sessionId = opts.session;
    if (!sessionId) {
      return {
        sessionId: null,
        totalUSD: 0,
        totalTokens: 0,
        turnCount: 0,
        models: [],
        note: 'no session id provided',
      };
    }
    const turns = await loadSessionTurnsViaArchive(sessionId, opts.onLog);
    if (turns.length === 0) {
      return {
        sessionId,
        totalUSD: 0,
        totalTokens: 0,
        turnCount: 0,
        models: [],
        note: 'no turns recorded for this session yet',
      };
    }
    const pricing = await loadPricing();
    const models = new Set();
    let totalTokens = 0;
    const costs = [];
    for (const t of turns) {
      models.add(t.model);
      const u = t.usage;
      totalTokens +=
        (u.input ?? 0) +
        (u.output ?? 0) +
        (u.reasoning ?? 0) +
        (u.cacheRead ?? 0) +
        (u.cacheCreate5m ?? 0) +
        (u.cacheCreate1h ?? 0);
      const c = costForTurn(t, pricing);
      if (c) costs.push(c);
    }
    const total = sumCosts(costs);
    return {
      sessionId,
      totalUSD: Math.round(total.total * 1_000_000) / 1_000_000,
      totalTokens,
      turnCount: turns.length,
      models: [...models].sort(),
    };
  });
}

// Coverage flags `attributeHotspots` and the matching aggregators need.
// Records without `fidelity` (older ledger writers, foreign sources) are
// treated as best-effort full and pass the gate. Mirrors
// `ATTRIBUTION_REQUIRED` + `turnPassesCoverage` in the CLI.
const HOTSPOTS_ATTRIBUTION_REQUIRED = ['hasToolCalls', 'hasToolResultEvents'];

function turnPassesCoverage(turn, required) {
  const f = turn.fidelity;
  if (!f) return true;
  for (const key of required) {
    if (!f.coverage[key]) return false;
  }
  return true;
}

const VALID_HOTSPOTS_GROUP_BY = ['attribution', 'bash', 'bash-verb', 'file', 'subagent'];

// Expanded hotspots(): returns a discriminated union covering every shape the
// CLI's `burn hotspots --json` (and a few narrower programmatic cuts) need.
//
//   { kind: 'attribution' }                       — full per-axis aggregations
//   { kind: 'bash' | 'bash-verb' | 'file' |
//          'subagent' }                           — narrow to one aggregation
//   { kind: 'findings' }                          — pattern findings (when
//                                                   `patterns` is set)
//
// `groupBy` and `patterns` are mutually exclusive: passing `patterns` always
// returns the findings shape and `groupBy` is ignored.
//
// Pattern detectors that need extra data (Claude settings, tool-result events,
// on-disk ghost surface) are loaded lazily based on the requested patterns,
// the same way the CLI does — so passing only `['retry-loop']` won't pay for
// a settings.json read.
export async function hotspots(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    if (
      opts.groupBy !== undefined &&
      !VALID_HOTSPOTS_GROUP_BY.includes(opts.groupBy)
    ) {
      throw new Error(
        `invalid hotspots groupBy: ${JSON.stringify(opts.groupBy)} ` +
          `(expected one of: ${VALID_HOTSPOTS_GROUP_BY.join(', ')})`,
      );
    }

    const q = {};
    if (opts.session) q.sessionId = opts.session;
    if (opts.project) q.project = opts.project;
    const since = normalizeSince(opts.since);
    if (since) q.since = since;

    const turns = await loadTurnsViaArchive(q, opts.onLog);
    const pricing = await loadPricing();

    if (opts.patterns && opts.patterns.length > 0) {
      return runHotspotsFindings(turns, pricing, opts);
    }

    return runHotspotsAttribution(turns, pricing, opts);
  });
}

async function runHotspotsAttribution(turns, pricing, opts) {
  const eligible = [];
  const excluded = [];
  for (const t of turns) {
    if (turnPassesCoverage(t, HOTSPOTS_ATTRIBUTION_REQUIRED)) eligible.push(t);
    else excluded.push(t);
  }

  const fidelitySummary = summarizeFidelity(turns);

  // Refusal: nothing to attribute. Mirror the CLI's refused-shape so callers
  // can branch on `refused` without re-deriving the reason.
  if (turns.length > 0 && eligible.length === 0) {
    const refusalReason =
      `${turns.length}/${turns.length} turns lack tool-call/tool-result coverage required for hotspots attribution`;
    const groupBy = opts.groupBy ?? 'attribution';
    if (groupBy !== 'attribution') {
      return { kind: groupBy, rows: [], refused: true, refusalReason };
    }
    return {
      kind: 'attribution',
      turnsAnalyzed: 0,
      grandTotal: 0,
      attributedTotal: 0,
      unattributedTotal: 0,
      attributionDegraded: false,
      sessions: [],
      files: [],
      bashVerbs: [],
      bash: [],
      subagents: [],
      fidelity: {
        analyzed: 0,
        excluded: turns.length,
        summary: fidelitySummary,
        refused: true,
      },
      refused: true,
      refusalReason,
    };
  }

  const sessionIds = new Set(eligible.map((t) => t.sessionId));
  const userTurnsBySession = await bulkUserTurnsBySession(sessionIds, q_(opts));
  const result = attributeHotspots(eligible, { pricing, userTurnsBySession });

  const groupBy = opts.groupBy ?? 'attribution';
  if (groupBy === 'bash') {
    return { kind: 'bash', rows: aggregateByBash(result.attributions) };
  }
  if (groupBy === 'bash-verb') {
    return {
      kind: 'bash-verb',
      rows: aggregateByBashVerb(result.attributions, parseBashCommand),
    };
  }
  if (groupBy === 'file') {
    return { kind: 'file', rows: aggregateByFile(result.attributions) };
  }
  if (groupBy === 'subagent') {
    return { kind: 'subagent', rows: aggregateBySubagent(result.attributions) };
  }

  const files = aggregateByFile(result.attributions);
  const bashVerbs = aggregateByBashVerb(result.attributions, parseBashCommand);
  const bash = aggregateByBash(result.attributions);
  const subagents = aggregateBySubagent(result.attributions);
  const evenSplit = result.sessionTotals.filter(
    (s) => s.attributionMethod === 'even-split',
  ).length;
  const attributionDegraded =
    result.sessionTotals.length > 0 &&
    evenSplit / result.sessionTotals.length >= 0.5;

  return {
    kind: 'attribution',
    turnsAnalyzed: eligible.length,
    grandTotal: result.grandTotal,
    attributedTotal: result.attributedTotal,
    unattributedTotal: result.unattributedTotal,
    attributionDegraded,
    sessions: result.sessionTotals,
    files,
    bashVerbs,
    bash,
    subagents,
    fidelity: {
      analyzed: eligible.length,
      excluded: excluded.length,
      summary: fidelitySummary,
      refused: false,
    },
  };
}

async function runHotspotsFindings(turns, pricing, opts) {
  const wanted = new Set(opts.patterns);
  const findings = [];
  const userTurns = await queryUserTurns({ sessionId: opts.session });
  const userTurnsBySession = bucketBySession(userTurns);

  // Core patterns (retries, failures, edit-heavy, etc.) flow through
  // detectPatterns + findingsFromPatterns; non-matching kinds are filtered.
  const detected = detectPatterns(turns, { pricing, userTurnsBySession });
  for (const f of findingsFromPatterns(detected)) {
    if (wanted.has(f.kind)) findings.push(f);
  }

  // Side-channel detectors live outside detectPatterns. Each one reads its
  // own slice of state, so we run them lazily based on `wanted`.

  if (wanted.has('tool-output-bloat')) {
    const settings = [];
    const userLoaded = await loadClaudeSettings(userClaudeSettingsPath());
    if (userLoaded) settings.push(userLoaded);
    const projectLoaded = await loadClaudeSettings(projectClaudeSettingsPath());
    if (projectLoaded) settings.push(projectLoaded);
    const toolResultEvents = await queryToolResultEvents({ sessionId: opts.session });
    const bloats = detectToolOutputBloat({
      settings,
      toolResultEvents,
      userTurns,
      turns,
      pricing,
    });
    for (const b of bloats) findings.push(toolOutputBloatToFinding(b));
  }

  if (wanted.has('ghost-surface')) {
    const ghostInputs = await buildGhostSurfaceInputs(turns, pricing);
    const ghosts = await detectGhostSurface(ghostInputs);
    for (const g of ghosts) findings.push(ghostSurfaceToFinding(g));
  }

  if (wanted.has('tool-call-pattern')) {
    const patterns = detectToolCallPatterns(turns, { pricing });
    for (const p of patterns) findings.push(toolCallPatternToFinding(p));
  }

  return {
    kind: 'findings',
    findings,
    summary: summarizeFidelity(turns),
  };
}

// Build the ledger Query from SDK opts. Used by the bulk user-turn loader so
// it narrows by `since`/`source` during streaming rather than buffering the
// entire historical ledger. Mirrors the CLI's same trick.
function q_(opts) {
  const q = {};
  if (opts.session) q.sessionId = opts.session;
  if (opts.project) q.project = opts.project;
  const since = normalizeSince(opts.since);
  if (since) q.since = since;
  return q;
}

// One ledger pass + in-memory bucket. Mirrors the CLI's `bulkUserTurnsBySession`:
// the per-session form `queryUserTurns({sessionId})` re-streams the entire
// ledger.jsonl on every call, so we issue a single bulk pass here and bucket
// by sessionId. `since`/`source` are forwarded so the streaming filter narrows
// the in-memory buffer to the same window the eligible turns live in.
async function bulkUserTurnsBySession(sessionIds, q = {}) {
  const out = new Map();
  if (sessionIds.size === 0) return out;
  const filter = {};
  if (q.since !== undefined) filter.since = q.since;
  if (q.source !== undefined) filter.source = q.source;
  const all = await queryUserTurns(filter);
  for (const ut of all) {
    if (!sessionIds.has(ut.sessionId)) continue;
    const list = out.get(ut.sessionId);
    if (list) list.push(ut);
    else out.set(ut.sessionId, [ut]);
  }
  return out;
}

function bucketBySession(userTurns) {
  const out = new Map();
  for (const ut of userTurns) {
    const list = out.get(ut.sessionId);
    if (list) list.push(ut);
    else out.set(ut.sessionId, [ut]);
  }
  return out;
}

const VALID_OVERHEAD_KINDS = ['claude-md', 'agents-md'];

// Discover and parse overhead files for a project, returning the parsed files
// alongside the cost attribution (per-file and per-section). Shared by
// `overhead()` (report mode) and `overheadTrim()` (recommendations mode) so the
// discovery + ingest + query + attribution pipeline lives in one place.
async function gatherOverhead(opts = {}) {
  const projectPath = opts.project ? path.resolve(opts.project) : process.cwd();
  const kind = opts.kind;
  if (kind !== undefined && !VALID_OVERHEAD_KINDS.includes(kind)) {
    throw new Error(
      `invalid overhead kind: ${JSON.stringify(kind)} (expected one of: ${VALID_OVERHEAD_KINDS.join(', ')})`,
    );
  }

  let found = await findOverheadFiles(projectPath);
  if (kind) found = found.filter((f) => f.kind === kind);
  if (found.length === 0) {
    return { projectPath, files: [], attribution: null };
  }

  const files = [];
  for (const f of found) files.push(await loadOverheadFile(f));

  const resolved = resolveProject(projectPath);
  const q = { project: resolved.projectKey ?? projectPath };
  const normalizedSince = normalizeSince(opts.since);
  if (normalizedSince) q.since = normalizedSince;

  const turns = await loadTurnsViaArchive(q, opts.onLog);
  const pricing = await loadPricing();
  const attribution = attributeOverhead({ files, turns, pricing });
  return { projectPath, files, attribution };
}

export async function overhead(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const data = await gatherOverhead(opts);
    if (!data.attribution) {
      return { project: data.projectPath, files: [], perFile: [], grandTotal: 0 };
    }
    return {
      project: data.projectPath,
      files: data.files.map(({ file, parsed }) => ({
        kind: file.kind,
        path: file.path,
        appliesTo: file.appliesTo,
        totalLines: parsed.totalLines,
        bytes: parsed.bytes,
        tokens: parsed.tokens,
        sections: parsed.sections,
        groupingLevel: parsed.groupingLevel,
      })),
      perFile: data.attribution.perFile.map((p) => ({
        path: p.file.path,
        kind: p.file.kind,
        appliesTo: p.file.appliesTo,
        attribution: p.attribution,
      })),
      grandTotal: data.attribution.grandTotal,
    };
  });
}

export async function overheadTrim(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const data = await gatherOverhead(opts);
    const topPerFile = parseTopN(opts.top);
    const sinceLabel = opts.since ?? 'all time';
    if (!data.attribution) {
      return {
        project: data.projectPath,
        since: sinceLabel,
        recommendations: [],
        summary: {
          filesAnalyzed: 0,
          filesWithRecommendations: 0,
          totalRecommendations: 0,
          totalProjectedSavingsPerSession: 0,
          totalProjectedSavingsAcrossWindow: 0,
        },
      };
    }

    // The diff field is the unified-diff text the trim recommendation would
    // produce — heavy enough to opt out of but useful enough that the CLI's
    // --json mode always emits it. Keep that default; allow opts.includeDiff
    // === false to skip the file reads when a caller (e.g. a future MCP tool)
    // only wants the recommendation rows.
    const includeDiff = opts.includeDiff !== false;
    const textCache = new Map();
    const recommendations = [];
    let filesWithRecommendations = 0;

    for (const fileAttr of data.attribution.perFile) {
      const recs = buildTrimRecommendations(fileAttr.attribution, topPerFile);
      if (recs.length === 0) continue;
      filesWithRecommendations++;
      let text;
      if (includeDiff) {
        text = textCache.get(fileAttr.file.path);
        if (text === undefined) {
          text = await readFile(fileAttr.file.path, 'utf8');
          textCache.set(fileAttr.file.path, text);
        }
      }
      for (const rec of recs) {
        const entry = {
          file: toProjectRelativePath(fileAttr.file.path, data.projectPath),
          kind: fileAttr.file.kind,
          appliesTo: fileAttr.file.appliesTo,
          section: {
            heading: rec.section.heading,
            startLine: rec.section.startLine,
            endLine: rec.section.endLine,
            tokens: rec.section.tokens,
          },
          projectedSavings: {
            perSessionUsd: rec.projectedSavingsPerSession,
            acrossWindowUsd: rec.projectedSavingsAcrossWindow,
            tokens: rec.section.tokens,
            tokenShare: rec.tokenShare,
          },
        };
        if (includeDiff) {
          entry.diff = renderUnifiedDiffForRecommendation(
            fileAttr.file.path,
            text,
            rec,
            data.projectPath,
          );
        }
        recommendations.push(entry);
      }
    }

    return {
      project: data.projectPath,
      since: sinceLabel,
      recommendations,
      summary: {
        filesAnalyzed: data.files.length,
        filesWithRecommendations,
        totalRecommendations: recommendations.length,
        totalProjectedSavingsPerSession: recommendations.reduce(
          (sum, r) => sum + r.projectedSavings.perSessionUsd,
          0,
        ),
        totalProjectedSavingsAcrossWindow: recommendations.reduce(
          (sum, r) => sum + r.projectedSavings.acrossWindowUsd,
          0,
        ),
      },
    };
  });
}

function parseTopN(v) {
  if (typeof v !== 'number' || !Number.isFinite(v) || v <= 0) return 3;
  return Math.floor(v);
}

function toProjectRelativePath(filePath, projectPath) {
  const rel = path.relative(projectPath, filePath);
  const display = rel && !rel.startsWith('..') ? rel : filePath;
  return display.split(path.sep).join('/');
}
