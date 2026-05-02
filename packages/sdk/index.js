import { queryAll, queryUserTurns } from '@relayburn/ledger';
import { loadPricing, costForTurn, attributeHotspots, detectPatterns, findingsFromPatterns } from '@relayburn/analyze';
import { ingestAll } from '@relayburn/cli';

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
    const q = { sessionId: opts.session, project: opts.project, since: opts.since ? new Date(opts.since) : undefined };
    const turns = await queryAll(q);
    const pricing = await loadPricing();
    const byTool = new Map();
    const byModel = new Map();
    let totalTokens = 0;
    let totalCost = 0;

    for (const t of turns) {
      const c = costForTurn(t, pricing).total;
      const usage = t.usage.input + t.usage.output + t.usage.reasoning + t.usage.cacheRead + t.usage.cacheCreate;
      totalTokens += usage;
      totalCost += c;

      const model = byModel.get(t.model) ?? { model: t.model, tokens: 0, cost: 0 };
      model.tokens += usage;
      model.cost += c;
      byModel.set(t.model, model);

      for (const call of t.toolCalls) {
        const tool = byTool.get(call.tool) ?? { tool: call.tool, tokens: 0, cost: 0, count: 0 };
        tool.tokens += usage;
        tool.cost += c;
        tool.count += 1;
        byTool.set(call.tool, tool);
      }
    }

    return { totalTokens, totalCost, byTool: [...byTool.values()], byModel: [...byModel.values()] };
  });
}

export async function hotspots(opts = {}) {
  return withHome(opts.ledgerHome, async () => {
    const turns = await queryAll({ sessionId: opts.session });
    const userTurns = await queryUserTurns({ sessionId: opts.session });
    const attribution = attributeHotspots({ turns, userTurns });

    if (!opts.patterns || opts.patterns.length === 0) return attribution;

    const detected = detectPatterns({ turns, userTurns, hotspots: attribution });
    return findingsFromPatterns(detected).filter((f) => opts.patterns.includes(f.kind));
  });
}
