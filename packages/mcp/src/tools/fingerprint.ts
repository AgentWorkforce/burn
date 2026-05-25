import { fingerprint as sdkFingerprint } from '@relayburn/sdk';
import type { FingerprintResult as SdkFingerprintResult } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';

export interface FingerprintInput {
  sessionId?: string;
  project?: string;
}

export type FingerprintResult = SdkFingerprintResult;

export interface FingerprintDeps {
  /**
   * Override for the SDK's `fingerprint(...)` call. Tests inject a fake to
   * exercise the tool surface without touching the on-disk ledger.
   */
  fingerprint?: (opts: {
    session?: string;
    project?: string;
  }) => Promise<SdkFingerprintResult>;
}

/**
 * `burn__fingerprint` — cheap polling primitive over the ledger.
 *
 * Returns a `{count}:{maxMtimeUnix}:{totalBytes}` string borrowed from
 * agent-profiler's `/api/traces` version field. Clients store the
 * last-seen value and skip work when it's unchanged. See
 * AgentWorkforce/burn#440.
 */
export function createFingerprintTool(deps: FingerprintDeps = {}): ToolDefinition {
  const callFingerprint = deps.fingerprint ?? sdkFingerprint;
  return {
    name: 'burn__fingerprint',
    description:
      'Cheap polling primitive over the burn ledger. Returns ' +
      '`{count}:{maxMtimeUnix}:{totalBytes}` joined by colons. ' +
      "Clients keep the last-seen value and skip re-querying when it's " +
      'unchanged. Optionally scoped to a session id or project path. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        sessionId: {
          type: 'string',
          description:
            'Restrict to a single session_id. Mutually exclusive with project.',
        },
        project: {
          type: 'string',
          description:
            'Restrict to rows whose project path matches. Mutually exclusive with sessionId.',
        },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      const input = raw as FingerprintInput;
      if (input.sessionId !== undefined && input.project !== undefined) {
        throw new Error('fingerprint: pass at most one of sessionId / project');
      }
      const opts: { session?: string; project?: string } = {};
      if (input.sessionId !== undefined) opts.session = input.sessionId;
      if (input.project !== undefined) opts.project = input.project;
      const result = await callFingerprint(opts);
      return result;
    },
  };
}
