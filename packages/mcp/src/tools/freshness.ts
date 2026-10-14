import { ledgerFreshness as sdkLedgerFreshness } from '@relayburn/sdk';
import type { LedgerFreshness } from '@relayburn/sdk';

/** Probe for the ledger's last-write clock and staleness threshold. */
export type LedgerFreshnessProbe = () => Promise<LedgerFreshness>;

export type WithLedgerFreshness<T> = T & { ledgerFreshness: LedgerFreshness };

/** Description suffix shared by every ledger read tool. */
export const FRESHNESS_NOTE =
  ' The response includes ledgerFreshness; check ledgerFreshness.stale before relying on ledger reads.';

/**
 * Run a ledger read alongside the freshness probe and attach the probe's
 * result, so every read tool reports the same staleness contract.
 */
export async function withLedgerFreshness<T extends object>(
  read: Promise<T>,
  probe: LedgerFreshnessProbe = sdkLedgerFreshness,
): Promise<WithLedgerFreshness<T>> {
  const [result, ledgerFreshness] = await Promise.all([read, probe()]);
  return { ...result, ledgerFreshness };
}
