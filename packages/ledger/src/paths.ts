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

export function plansPath(): string {
  return path.join(ledgerHome(), 'plans.json');
}

export function archivePath(): string {
  return path.join(ledgerHome(), 'archive.sqlite');
}

// Default location for the consolidated SQLite store used by SqliteAdapter
// (selected via `RELAYBURN_STORAGE=sqlite`). Overridable via
// `RELAYBURN_SQLITE_PATH` for callers that want the DB on a separate volume
// from the rest of `RELAYBURN_HOME` (e.g. a durable mount in a sandbox where
// the home directory is ephemeral).
export function sqlitePath(): string {
  const env = process.env['RELAYBURN_SQLITE_PATH'];
  if (env && env.length > 0) return env;
  return path.join(ledgerHome(), 'burn.sqlite');
}

export function contentDir(): string {
  return path.join(ledgerHome(), 'content');
}

// Allow the character set observed in real session IDs: UUIDs (Claude),
// `ses_<base>` (opencode), `sess_<base>` + codex `turn_*` IDs, hyphens and
// underscores. Reject anything that could escape the content directory.
const SESSION_ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

// Cap well below the common 255-byte filesystem filename limit so
// `${sessionId}.jsonl` and `content.${sessionId}.lock` (see lockPath) never
// overflow. Real session IDs are UUIDs or short `ses_*`/`sess_*`/`turn_*`
// strings — all comfortably under this bound.
const MAX_SESSION_ID_LENGTH = 128;

export function isValidSessionId(sessionId: string): boolean {
  if (sessionId.length === 0 || sessionId.length > MAX_SESSION_ID_LENGTH) return false;
  if (sessionId === '.' || sessionId === '..') return false;
  return SESSION_ID_RE.test(sessionId);
}

export function contentFilePath(sessionId: string): string {
  if (!isValidSessionId(sessionId)) {
    throw new Error(`invalid sessionId for content path: ${JSON.stringify(sessionId)}`);
  }
  return path.join(contentDir(), `${sessionId}.jsonl`);
}
