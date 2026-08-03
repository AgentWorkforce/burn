import { compare as sdkCompare } from '@relayburn/sdk';
import type { CompareOptions, CompareResult, FidelityClass } from '@relayburn/sdk';

import type { ToolDefinition } from '../types.js';
import {
  optionalEnum,
  optionalNonNegativeInteger,
  optionalString,
  optionalStringArray,
  requiredStringArray,
  validateObjectInput,
} from './input.js';

export interface CompareInput {
  models: string[];
  session?: string;
  project?: string;
  since?: string;
  workflow?: string;
  agent?: string;
  provider?: string[];
  minSample?: number;
  minFidelity?: FidelityClass;
}

export type { CompareResult } from '@relayburn/sdk';

export interface CompareDeps {
  compare?: (opts: CompareOptions) => Promise<CompareResult>;
}

const FIDELITY = ['full', 'usage-only', 'aggregate-only', 'cost-only', 'partial'] as const;
const PROPERTIES = ['models', 'session', 'project', 'since', 'workflow', 'agent', 'provider', 'minSample', 'minFidelity'] as const;

export function createCompareTool(deps: CompareDeps = {}): ToolDefinition {
  const callCompare = deps.compare ?? sdkCompare;
  return {
    name: 'burn__compare',
    description:
      'Compare cost and outcome metrics across at least two models, grouped by activity category. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        models: { type: 'array', items: { type: 'string' }, minItems: 2, description: 'Model names to compare.' },
        session: { type: 'string', description: 'Restrict to one session id.' },
        project: { type: 'string', description: 'Restrict to one project path or key.' },
        since: { type: 'string', description: 'ISO timestamp or relative range such as 24h or 7d.' },
        workflow: { type: 'string', description: 'Restrict to a folded workflowId enrichment stamp.' },
        agent: { type: 'string', description: 'Restrict to a folded agentId enrichment stamp.' },
        provider: { type: 'array', items: { type: 'string' }, description: 'Case-insensitive provider allow-list.' },
        minSample: { type: 'integer', minimum: 0, description: 'Minimum observations before a comparison cell is sufficient.' },
        minFidelity: { type: 'string', enum: FIDELITY, description: 'Minimum accepted telemetry fidelity.' },
      },
      required: ['models'],
      additionalProperties: false,
    },
    handler: async (raw) => {
      validateObjectInput(raw, 'compare', PROPERTIES);
      const opts: CompareOptions = { models: requiredStringArray(raw, 'models', 'compare', 2) };
      const session = optionalString(raw, 'session', 'compare');
      const project = optionalString(raw, 'project', 'compare');
      const since = optionalString(raw, 'since', 'compare');
      const workflow = optionalString(raw, 'workflow', 'compare');
      const agent = optionalString(raw, 'agent', 'compare');
      const provider = optionalStringArray(raw, 'provider', 'compare');
      const minSample = optionalNonNegativeInteger(raw, 'minSample', 'compare');
      const minFidelity = optionalEnum(raw, 'minFidelity', 'compare', FIDELITY);
      if (session !== undefined) opts.session = session;
      if (project !== undefined) opts.project = project;
      if (since !== undefined) opts.since = since;
      if (workflow !== undefined) opts.workflow = workflow;
      if (agent !== undefined) opts.agent = agent;
      if (provider !== undefined) opts.provider = provider;
      if (minSample !== undefined) opts.minSample = minSample;
      if (minFidelity !== undefined) opts.minFidelity = minFidelity;
      return callCompare(opts);
    },
  };
}
