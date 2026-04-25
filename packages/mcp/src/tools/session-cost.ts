import { costForTurn, loadPricing, sumCosts } from '@relayburn/analyze';
import type { PricingTable } from '@relayburn/analyze';
import { queryAll } from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';

import type { ToolDefinition } from '../types.js';

export interface SessionCostInput {
  sessionId?: string;
}

export interface SessionCostResult {
  sessionId: string | null;
  totalUSD: number;
  totalTokens: number;
  turnCount: number;
  models: string[];
  note?: string;
}

export interface SessionCostDeps {
  defaultSessionId: string | undefined;
  queryTurns?: (sessionId: string) => Promise<EnrichedTurn[]>;
  loadPricing?: () => Promise<PricingTable>;
}

export function createSessionCostTool(deps: SessionCostDeps): ToolDefinition {
  const queryTurns = deps.queryTurns ?? ((id) => queryAll({ sessionId: id }));
  const pricingLoader = deps.loadPricing ?? loadPricing;

  return {
    name: 'burn__sessionCost',
    description:
      'Return the total cost (USD), token count, and turn count for a session. ' +
      "Defaults to the server's registered sessionId (the running agent's own " +
      'session). Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        sessionId: {
          type: 'string',
          description:
            "Override the registered session id. Omit to query the running agent's own session.",
        },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      const input = raw as SessionCostInput;
      const sessionId = input.sessionId ?? deps.defaultSessionId;
      if (!sessionId) {
        return {
          sessionId: null,
          totalUSD: 0,
          totalTokens: 0,
          turnCount: 0,
          models: [],
          note: 'no session id provided and server was not registered with one',
        } satisfies SessionCostResult;
      }
      const turns = await queryTurns(sessionId);
      if (turns.length === 0) {
        return {
          sessionId,
          totalUSD: 0,
          totalTokens: 0,
          turnCount: 0,
          models: [],
          note: 'no turns recorded for this session yet',
        } satisfies SessionCostResult;
      }
      const pricing = await pricingLoader();
      const models = new Set<string>();
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
        totalUSD: round6(total.total),
        totalTokens,
        turnCount: turns.length,
        models: [...models].sort(),
      } satisfies SessionCostResult;
    },
  };
}

function round6(n: number): number {
  return Math.round(n * 1_000_000) / 1_000_000;
}
