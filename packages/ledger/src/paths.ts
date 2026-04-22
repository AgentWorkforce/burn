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

export function cursorsPath(): string {
  return path.join(ledgerHome(), 'cursors.json');
}

export function ledgerIndexPath(): string {
  return path.join(ledgerHome(), 'ledger.idx');
}

export function ledgerContentIndexPath(): string {
  return path.join(ledgerHome(), 'ledger.content.idx');
}

export function lockPath(name: string): string {
  return path.join(ledgerHome(), `${name}.lock`);
}

export function pricingOverridePath(): string {
  return path.join(ledgerHome(), 'models.dev.json');
}

export function configPath(): string {
  return path.join(ledgerHome(), 'config.json');
}

export function contentDir(): string {
  return path.join(ledgerHome(), 'content');
}

export function contentFilePath(sessionId: string): string {
  return path.join(contentDir(), `${sessionId}.jsonl`);
}
