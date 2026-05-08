import { sessionCost as sdkSessionCost } from '@relayburn/sdk';
import type { SessionCostResult as SdkSessionCostResult } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';

export interface SessionCostInput {
  sessionId?: string;
}

export type SessionCostResult = SdkSessionCostResult;

export interface SessionCostDeps {
  defaultSessionId: string | undefined;
  /**
   * Override for the SDK's `sessionCost(...)` call. Tests inject a fake to
   * exercise the tool surface without touching the on-disk ledger.
   */
  sessionCost?: (opts: { session?: string }) => Promise<SdkSessionCostResult>;
}

export function createSessionCostTool(deps: SessionCostDeps): ToolDefinition {
  const callSessionCost = deps.sessionCost ?? sdkSessionCost;
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
      const opts: { session?: string } = {};
      if (sessionId !== undefined) opts.session = sessionId;
      const result = await callSessionCost(opts);
      // The SDK's "no session id" note is generic ("no session id provided");
      // keep the more descriptive variant the MCP tool used to surface so the
      // hint that the *server* should have been registered with one stays
      // visible to MCP clients.
      if (result.sessionId === null && sessionId === undefined) {
        return { ...result, note: 'no session id provided and server was not registered with one' };
      }
      return result;
    },
  };
}
