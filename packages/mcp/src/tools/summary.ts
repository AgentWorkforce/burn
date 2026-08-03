import { summary as sdkSummary } from '@relayburn/sdk';
import type { SummaryOptions } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';
import {
  optionalString,
  optionalStringRecord,
  validateObjectInput,
} from './input.js';

export interface SummaryInput {
  session?: string;
  project?: string;
  since?: string;
  tags?: Record<string, string>;
  groupByTag?: string;
}

export type SummaryResult = Awaited<ReturnType<typeof sdkSummary>>;

export interface SummaryDeps {
  defaultSessionId: string | undefined;
  summary?: (opts: SummaryOptions) => Promise<SummaryResult>;
}

const PROPERTIES = ['session', 'project', 'since', 'tags', 'groupByTag'] as const;

export function createSummaryTool(deps: SummaryDeps): ToolDefinition {
  const callSummary = deps.summary ?? sdkSummary;
  return {
    name: 'burn__summary',
    description:
      'Summarize token use and cost by tool and model, optionally filtered by session, project, time window, or enrichment tags. When the server has a registered default session, omitting session restricts the query to it. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        session: {
          type: 'string',
          description: 'Restrict to one session id. Omit to use the server registered session when present.',
        },
        project: { type: 'string', description: 'Restrict to one project path or key.' },
        since: { type: 'string', description: 'ISO timestamp or relative range such as 24h or 7d.' },
        tags: {
          type: 'object',
          additionalProperties: { type: 'string' },
          description: 'Folded enrichment tags; every key/value pair must match.',
        },
        groupByTag: { type: 'string', description: 'Group totals by this folded enrichment tag key.' },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      validateObjectInput(raw, 'summary', PROPERTIES);
      const opts: SummaryOptions = {};
      const session = optionalString(raw, 'session', 'summary') ?? deps.defaultSessionId;
      const project = optionalString(raw, 'project', 'summary');
      const since = optionalString(raw, 'since', 'summary');
      const tags = optionalStringRecord(raw, 'tags', 'summary');
      const groupByTag = optionalString(raw, 'groupByTag', 'summary');
      if (session !== undefined) opts.session = session;
      if (project !== undefined) opts.project = project;
      if (since !== undefined) opts.since = since;
      if (tags !== undefined) opts.tags = tags;
      if (groupByTag !== undefined) opts.groupByTag = groupByTag;
      return callSummary(opts);
    },
  };
}
