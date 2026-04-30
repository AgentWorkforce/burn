import { readFile } from 'node:fs/promises';

import type { ContentStoreMode } from '@relayburn/reader';

import { configPath } from './paths.js';

export interface ContentConfig {
  store: ContentStoreMode;
  retentionDays: number | 'forever';
}

export interface BurnConfig {
  content: ContentConfig;
}

const DEFAULT_RETENTION_DAYS = 90;

export const DEFAULT_CONFIG: BurnConfig = {
  content: { store: 'full', retentionDays: DEFAULT_RETENTION_DAYS },
};

interface RawConfig {
  content?: {
    store?: unknown;
    retentionDays?: unknown;
  };
}

export async function loadConfig(): Promise<BurnConfig> {
  const fromFile = await readConfigFile();
  const store = pickStore(
    process.env['RELAYBURN_CONTENT_STORE'],
    fromFile?.content?.store,
    DEFAULT_CONFIG.content.store,
  );
  const retentionDays = pickRetention(
    process.env['RELAYBURN_CONTENT_TTL_DAYS'],
    fromFile?.content?.retentionDays,
    DEFAULT_CONFIG.content.retentionDays,
  );
  return { content: { store, retentionDays } };
}

async function readConfigFile(): Promise<RawConfig | null> {
  let raw: string;
  try {
    raw = await readFile(configPath(), 'utf8');
  } catch (err) {
    // Missing config file is the common case and not worth mentioning.
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') {
      process.stderr.write(
        `[burn] warning: could not read ${configPath()}: ${(err as Error).message}\n`,
      );
    }
    return null;
  }
  try {
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed === 'object') return parsed as RawConfig;
    process.stderr.write(
      `[burn] warning: ${configPath()} is not a JSON object; using defaults\n`,
    );
  } catch (err) {
    process.stderr.write(
      `[burn] warning: invalid JSON in ${configPath()} (${(err as Error).message}); using defaults\n`,
    );
  }
  return null;
}

function pickStore(
  env: string | undefined,
  fromFile: unknown,
  fallback: ContentStoreMode,
): ContentStoreMode {
  const envMode = normalizeStore(env);
  if (envMode !== null) return envMode;
  const fileMode = normalizeStore(fromFile);
  if (fileMode !== null) return fileMode;
  return fallback;
}

function normalizeStore(v: unknown): ContentStoreMode | null {
  if (typeof v !== 'string') return null;
  const s = v.toLowerCase();
  if (s === 'full' || s === 'hash-only' || s === 'off') return s;
  return null;
}

function pickRetention(
  env: string | undefined,
  fromFile: unknown,
  fallback: number | 'forever',
): number | 'forever' {
  const envRet = normalizeRetention(env);
  if (envRet !== null) return envRet;
  const fileRet = normalizeRetention(fromFile);
  if (fileRet !== null) return fileRet;
  return fallback;
}

function normalizeRetention(v: unknown): number | 'forever' | null {
  if (typeof v === 'number' && Number.isFinite(v)) {
    if (v < 0) return 'forever';
    return v;
  }
  if (typeof v === 'string') {
    const s = v.trim().toLowerCase();
    // Empty string means "not set" — important because `RELAYBURN_CONTENT_TTL_DAYS=`
    // (or a CI/CD pipeline producing an empty value) would otherwise parse as
    // `Number('') === 0` and silently configure a zero-day retention.
    if (s === '') return null;
    if (s === 'forever') return 'forever';
    const n = Number(s);
    if (Number.isFinite(n)) {
      if (n < 0) return 'forever';
      return n;
    }
  }
  return null;
}

export function retentionMs(r: number | 'forever'): number | null {
  if (r === 'forever') return null;
  return r * 24 * 60 * 60 * 1000;
}
