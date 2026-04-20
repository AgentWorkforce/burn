import { readFile } from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

import { pricingOverridePath } from '@relayburn/ledger';

export interface ModelCost {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

export type PricingTable = Record<string, ModelCost>;

interface ModelsDevModel {
  id?: string;
  cost?: {
    input?: number;
    output?: number;
    cache_read?: number;
    cache_write?: number;
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

function flatten(root: ModelsDevRoot): PricingTable {
  const out: PricingTable = {};
  for (const provider of Object.values(root)) {
    const models = provider.models;
    if (!models) continue;
    for (const [id, model] of Object.entries(models)) {
      const cost = model.cost;
      if (!cost || typeof cost.input !== 'number' || typeof cost.output !== 'number') continue;
      out[id] = {
        input: cost.input,
        output: cost.output,
        cacheRead: cost.cache_read ?? 0,
        cacheWrite: cost.cache_write ?? cost.input,
      };
    }
  }
  return out;
}
