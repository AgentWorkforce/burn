import { overhead as sdkOverhead } from '@relayburn/sdk';
import type { OverheadFileKind, OverheadOptions, OverheadResult } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';
import { optionalEnum, optionalString, validateObjectInput } from './input.js';

export interface OverheadInput {
  project?: string;
  since?: string;
  kind?: OverheadFileKind;
}

export type { OverheadResult } from '@relayburn/sdk';

export interface OverheadDeps {
  overhead?: (opts: OverheadOptions) => Promise<OverheadResult>;
}

const KINDS = ['claude-md', 'agents-md'] as const;
const PROPERTIES = ['project', 'since', 'kind'] as const;

export function createOverheadTool(deps: OverheadDeps = {}): ToolDefinition {
  const callOverhead = deps.overhead ?? sdkOverhead;
  return {
    name: 'burn__overhead',
    description:
      'Attribute CLAUDE.md and AGENTS.md instruction-file token overhead and cost by file and section. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        project: { type: 'string', description: 'Project path or key. The SDK defaults to the current project.' },
        since: { type: 'string', description: 'ISO timestamp or relative range such as 24h or 7d.' },
        kind: { type: 'string', enum: KINDS, description: 'Restrict to one instruction-file kind.' },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      validateObjectInput(raw, 'overhead', PROPERTIES);
      const opts: OverheadOptions = {};
      const project = optionalString(raw, 'project', 'overhead');
      const since = optionalString(raw, 'since', 'overhead');
      const kind = optionalEnum(raw, 'kind', 'overhead', KINDS);
      if (project !== undefined) opts.project = project;
      if (since !== undefined) opts.since = since;
      if (kind !== undefined) opts.kind = kind;
      return callOverhead(opts);
    },
  };
}
