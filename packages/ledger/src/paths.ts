import { homedir } from 'node:os';
import * as path from 'node:path';

export function ledgerHome(): string {
  const env = process.env['RELAYBURN_HOME'];
  if (env && env.length > 0) return env;
  return path.join(homedir(), '.relayburn');
}

export function ledgerPath(): string {
  return path.join(ledgerHome(), 'ledger.jsonl');
}

export function hwmPath(): string {
  return path.join(ledgerHome(), 'hwm.json');
}

export function pricingOverridePath(): string {
  return path.join(ledgerHome(), 'models.dev.json');
}
