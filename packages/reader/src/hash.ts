import { createHash } from 'node:crypto';

export function stableStringify(value: unknown): string {
  if (value === null || typeof value !== 'object') {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return '[' + value.map(stableStringify).join(',') + ']';
  }
  const obj = value as Record<string, unknown>;
  const keys = Object.keys(obj).sort();
  return '{' + keys.map((k) => JSON.stringify(k) + ':' + stableStringify(obj[k])).join(',') + '}';
}

export function argsHash(input: unknown): string {
  return createHash('sha256').update(stableStringify(input)).digest('hex').slice(0, 16);
}

// Short hash of a raw string (Edit old_string / new_string, Write content).
// Uses the same 16-char truncation as argsHash so detector output is visually
// consistent. Never call with undefined — pre/post may be legitimately empty
// and the empty-string hash is meaningful.
export function contentHash(s: string): string {
  return createHash('sha256').update(s).digest('hex').slice(0, 16);
}
