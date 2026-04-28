import {
  filterTurnsByProvider as filterTurnsByProviderShared,
  resolveTurnProvider,
} from '@relayburn/analyze';
import type { ProviderFilter, TurnProvider } from '@relayburn/analyze';
import type { TurnRecord } from '@relayburn/reader';

export { resolveTurnProvider };
export type { ProviderFilter, TurnProvider };

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
  return filterTurnsByProviderShared(turns, filter);
}
