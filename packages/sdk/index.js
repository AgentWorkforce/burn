import { queryAll, queryUserTurns, queryToolResultEvents } from '@relayburn/ledger';
import {
  loadPricing,
  costForTurn,
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
import { ingestAll, buildGhostSurfaceInputs } from '@relayburn/cli';

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
    const turns = await queryAll(q);
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

    return { totalTokens, totalCost, byTool: [...byTool.values()], byModel: [...byModel.values()] };
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
