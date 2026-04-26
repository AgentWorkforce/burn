import { resolveProvider } from '@relayburn/analyze';
import type { SourceKind, TurnRecord } from '@relayburn/reader';

export interface TurnProvider {
  provider: string;
  rawModel: string;
  normalizedModel: string;
  matchedRule?: string;
}

export type ProviderFilter = Set<string>;

export function resolveTurnProvider(
  turn: Pick<TurnRecord, 'model' | 'source'>,
): TurnProvider {
  const resolved = resolveProvider(turn.model);
  if (resolved.provider) {
    const out: TurnProvider = {
      provider: resolved.provider,
      rawModel: turn.model,
      normalizedModel: resolved.normalizedModel,
    };
    if (resolved.matchedRule) out.matchedRule = resolved.matchedRule;
    return out;
  }

  const providerPrefix = providerFromModelPrefix(turn.model);
  if (providerPrefix) {
    return {
      provider: providerPrefix,
      rawModel: turn.model,
      normalizedModel: stripProviderPrefix(turn.model),
    };
  }

  return {
    provider: providerFromSource(turn.source),
    rawModel: turn.model,
    normalizedModel: turn.model,
  };
}

export function parseProviderFilter(
  flag: string | true | undefined,
): ProviderFilter | undefined | Error {
  if (flag === undefined) return undefined;
  if (flag === true) return new Error('burn: --provider requires a value\n');
  const providers = flag
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  if (providers.length === 0) return new Error('burn: --provider requires a value\n');
  return new Set(providers);
}

export function filterTurnsByProvider<T extends Pick<TurnRecord, 'model' | 'source'>>(
  turns: T[],
  filter: ProviderFilter | undefined,
): T[] {
  if (!filter) return turns;
  return turns.filter((t) => filter.has(resolveTurnProvider(t).provider.toLowerCase()));
}

function providerFromModelPrefix(model: string): string | undefined {
  const i = model.indexOf('/');
  if (i <= 0) return undefined;
  return model.slice(0, i).toLowerCase();
}

function stripProviderPrefix(model: string): string {
  const i = model.indexOf('/');
  return i >= 0 ? model.slice(i + 1) : model;
}

function providerFromSource(source: SourceKind): string {
  switch (source) {
    case 'claude-code':
    case 'anthropic-api':
      return 'anthropic';
    case 'codex':
    case 'openai-api':
      return 'openai';
    case 'gemini-api':
      return 'google';
    case 'opencode':
    default:
      return source;
  }
}
