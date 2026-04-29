import type { StorageAdapter, StorageAdapterKind } from './adapter.js';
import { FileAdapter } from './file-adapter.js';

let adapter: StorageAdapter | undefined;

export function getAdapter(): StorageAdapter {
  if (adapter) return adapter;
  const kind = getStorageAdapterKind();
  switch (kind) {
    case 'file':
      adapter = new FileAdapter();
      return adapter;
    case 'sqlite':
    case 'postgres':
    case 'http':
      throw new Error(`RELAYBURN_STORAGE=${kind} is not supported in this build`);
  }
}

export function getStorageAdapterKind(): StorageAdapterKind {
  const raw = process.env['RELAYBURN_STORAGE'] ?? 'file';
  if (raw === 'file' || raw === 'sqlite' || raw === 'postgres' || raw === 'http') {
    return raw;
  }
  throw new Error(
    `unsupported RELAYBURN_STORAGE=${JSON.stringify(raw)} ` +
      '(expected file, sqlite, postgres, or http)',
  );
}

export function __resetAdapterForTesting(): void {
  adapter = undefined;
}
