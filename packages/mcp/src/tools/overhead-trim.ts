import { overheadTrim as sdkOverheadTrim } from '@relayburn/sdk';
import type { OverheadFileKind, OverheadTrimOptions, OverheadTrimResult } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';
import {
  optionalBoolean,
  optionalEnum,
  optionalNonNegativeInteger,
  optionalString,
  validateObjectInput,
} from './input.js';

export interface OverheadTrimInput {
  project?: string;
  since?: string;
  kind?: OverheadFileKind;
  top?: number;
  includeDiff?: boolean;
}

export type { OverheadTrimResult } from '@relayburn/sdk';

export interface OverheadTrimDeps {
  overheadTrim?: (opts: OverheadTrimOptions) => Promise<OverheadTrimResult>;
}

const KINDS = ['claude-md', 'agents-md'] as const;
const PROPERTIES = ['project', 'since', 'kind', 'top', 'includeDiff'] as const;

export function createOverheadTrimTool(deps: OverheadTrimDeps = {}): ToolDefinition {
  const callOverheadTrim = deps.overheadTrim ?? sdkOverheadTrim;
  return {
    name: 'burn__overheadTrim',
    description:
      'Recommend high-cost instruction-file sections to trim and estimate their savings, optionally with suggested diffs. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        project: { type: 'string', description: 'Project path or key. The SDK defaults to the current project.' },
        since: { type: 'string', description: 'ISO timestamp or relative range such as 24h or 7d.' },
        kind: { type: 'string', enum: KINDS, description: 'Restrict to one instruction-file kind.' },
        top: { type: 'integer', minimum: 0, description: 'Maximum number of recommendations.' },
        includeDiff: { type: 'boolean', description: 'Include a suggested edit diff for each recommendation.' },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async (raw) => {
      validateObjectInput(raw, 'overhead trim', PROPERTIES);
      const opts: OverheadTrimOptions = {};
      const project = optionalString(raw, 'project', 'overhead trim');
      const since = optionalString(raw, 'since', 'overhead trim');
      const kind = optionalEnum(raw, 'kind', 'overhead trim', KINDS);
      const top = optionalNonNegativeInteger(raw, 'top', 'overhead trim');
      const includeDiff = optionalBoolean(raw, 'includeDiff', 'overhead trim');
      if (project !== undefined) opts.project = project;
      if (since !== undefined) opts.since = since;
      if (kind !== undefined) opts.kind = kind;
      if (top !== undefined) opts.top = top;
      if (includeDiff !== undefined) opts.includeDiff = includeDiff;
      return callOverheadTrim(opts);
    },
  };
}
