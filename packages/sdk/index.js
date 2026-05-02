import {
  buildArchive,
  queryAll,
  queryAllFromArchive,
  queryTurnsFromArchive,
  queryUserTurns,
  queryToolResultEvents,
} from '@relayburn/ledger';
import {
  buildGhostSurfaceInputs,
  loadPricing,
  costForTurn,
  sumCosts,
  attributeHotspots,
  detectPatterns,
  findingsFromPatterns,
  detectToolOutputBloat,
  toolOutputBloatToFinding,
  detectGhostSurface,
  ghostSurfaceToFinding,
  detectToolCallPatterns,
  toolCallPatternToFinding,
  loadClaudeSettings,
  userClaudeSettingsPath,
  projectClaudeSettingsPath,
} from '@relayburn/analyze';
import { ingestAll } from '@relayburn/ingest';

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
    const q = { sessionId: opts.session, project: opts.project, since: opts.since };
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

export async function hotspots(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const turns = await queryAll({ sessionId: opts.session });
    const userTurns = await queryUserTurns({ sessionId: opts.session });
    const pricing = await loadPricing();
    const userTurnsBySession = bucketBySession(userTurns);
    const attribution = attributeHotspots(turns, { pricing, userTurnsBySession });

    if (!opts.patterns || opts.patterns.length === 0) return attribution;

    const wanted = new Set(opts.patterns);
    const findings = [];

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

    return findings;
  });
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
