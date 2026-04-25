import { readFile } from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

import { pricingOverridePath } from '@relayburn/ledger';

/**
 * How a model's reasoning tokens should be priced.
 *
 * - `included_in_output`: The harness/source already counts reasoning tokens
 *   inside `output_tokens`, so `usage.reasoning` is informational only and
 *   must NOT be billed on top of `usage.output`. Codex transcripts behave
 *   this way.
 * - `separate`: The model has a distinct reasoning tariff (`cost.reasoning`
 *   in the `models.dev` snapshot). Bill `usage.reasoning` at that tariff.
 * - `same_as_output`: `usage.output` and `usage.reasoning` are non-overlapping
 *   token buckets and there is no distinct reasoning tariff. Bill
 *   `usage.reasoning` at the output rate. Anthropic Claude transcripts are
 *   the canonical example.
 */
export type ReasoningMode = 'included_in_output' | 'separate' | 'same_as_output';

export interface ModelCost {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  /** Per-million reasoning-token tariff. Set iff `reasoningMode === 'separate'`. */
  reasoning?: number;
  reasoningMode: ReasoningMode;
}

export type PricingTable = Record<string, ModelCost>;

interface ModelsDevModel {
  id?: string;
  cost?: {
    input?: number;
    output?: number;
    cache_read?: number;
    cache_write?: number;
    reasoning?: number;
  };
}

interface ModelsDevProvider {
  id?: string;
  models?: Record<string, ModelsDevModel>;
}

type ModelsDevRoot = Record<string, ModelsDevProvider>;

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const BUILTIN_PRICING = path.resolve(__dirname, '..', 'pricing', 'models.dev.json');

export async function loadBuiltinPricing(): Promise<PricingTable> {
  return loadFromFile(BUILTIN_PRICING);
}

export async function loadPricing(overridePath?: string): Promise<PricingTable> {
  const builtin = await loadBuiltinPricing();
  const override = overridePath ?? pricingOverridePath();
  try {
    const user = await loadFromFile(override);
    return { ...builtin, ...user };
  } catch {
    return builtin;
  }
}

async function loadFromFile(filePath: string): Promise<PricingTable> {
  const raw = await readFile(filePath, 'utf8');
  const parsed = JSON.parse(raw) as ModelsDevRoot;
  return flatten(parsed);
}

export function flatten(root: ModelsDevRoot): PricingTable {
  const out: PricingTable = {};
  for (const provider of Object.values(root)) {
    const models = provider.models;
    if (!models) continue;
    for (const [id, model] of Object.entries(models)) {
      const cost = model.cost;
      if (!cost || typeof cost.input !== 'number' || typeof cost.output !== 'number') continue;
      const hasReasoning = typeof cost.reasoning === 'number';
      const entry: ModelCost = {
        input: cost.input,
        output: cost.output,
        cacheRead: cost.cache_read ?? 0,
        cacheWrite: cost.cache_write ?? cost.input,
        reasoningMode: hasReasoning ? 'separate' : 'same_as_output',
      };
      if (hasReasoning && typeof cost.reasoning === 'number') {
        entry.reasoning = cost.reasoning;
      }
      out[id] = entry;
    }
  }
  return out;
}
