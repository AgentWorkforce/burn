import { hotspots as sdkHotspots } from '@relayburn/sdk';
import type { HotspotsGroupBy, HotspotsOptions, HotspotsResult } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';
import {
  optionalEnum,
  optionalString,
  optionalStringArray,
  validateObjectInput,
} from './input.js';

export interface HotspotsInput {
  session?: string;
  project?: string;
  since?: string;
  groupBy?: HotspotsGroupBy;
  patterns?: string[];
  workflow?: string;
  provider?: string[];
}

export type { HotspotsResult } from '@relayburn/sdk';

export interface HotspotsDeps {
  defaultSessionId: string | undefined;
  hotspots?: (opts: HotspotsOptions) => Promise<HotspotsResult>;
}

const GROUP_BY = ['attribution', 'bash', 'bash-verb', 'file', 'subagent', 'findings'] as const;
const PROPERTIES = ['session', 'project', 'since', 'groupBy', 'patterns', 'workflow', 'provider'] as const;

export function createHotspotsTool(deps: HotspotsDeps): ToolDefinition {
  const callHotspots = deps.hotspots ?? sdkHotspots;
  return {
    name: 'burn__hotspots',
    description:
      'Find expensive tool-output persistence and repeated workflow patterns, with attribution or grouped findings views. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        session: { type: 'string', description: 'Restrict to one session id.' },
        project: { type: 'string', description: 'Restrict to one project path or key.' },
        since: { type: 'string', description: 'ISO timestamp or relative range such as 24h or 7d.' },
        groupBy: { type: 'string', enum: GROUP_BY, description: 'Select the hotspot result view.' },
        patterns: { type: 'array', items: { type: 'string' }, description: 'Only include matching finding patterns.' },
        workflow: { type: 'string', description: 'Restrict to a folded workflowId enrichment stamp.' },
        provider: { type: 'array', items: { type: 'string' }, description: 'Case-insensitive provider allow-list.' },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      validateObjectInput(raw, 'hotspots', PROPERTIES);
      const opts: HotspotsOptions = {};
      const session = optionalString(raw, 'session', 'hotspots') ?? deps.defaultSessionId;
      const project = optionalString(raw, 'project', 'hotspots');
      const since = optionalString(raw, 'since', 'hotspots');
      const groupBy = optionalEnum(raw, 'groupBy', 'hotspots', GROUP_BY);
      const patterns = optionalStringArray(raw, 'patterns', 'hotspots');
      const workflow = optionalString(raw, 'workflow', 'hotspots');
      const provider = optionalStringArray(raw, 'provider', 'hotspots');
      if (session !== undefined) opts.session = session;
      if (project !== undefined) opts.project = project;
      if (since !== undefined) opts.since = since;
      if (groupBy !== undefined) opts.groupBy = groupBy;
      if (patterns !== undefined) opts.patterns = patterns;
      if (workflow !== undefined) opts.workflow = workflow;
      if (provider !== undefined) opts.provider = provider;
      return callHotspots(opts);
    },
  };
}
