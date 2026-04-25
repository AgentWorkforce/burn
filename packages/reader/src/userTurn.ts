// Shared helpers for building UserTurnBlock entries across the Claude / Codex /
// OpenCode parsers. The token estimate uses the bytes/4 heuristic noted in
// issue #2 — the constant cancels for proportional allocation across blocks
// within the same user turn, and a real tokenizer can be wired in later if
// downstream consumers need accuracy.

import type { UserTurnBlock } from './types.js';

export function makeTextBlock(text: string): UserTurnBlock {
  const byteLen = Buffer.byteLength(text, 'utf8');
  return { kind: 'text', byteLen, approxTokens: bytesToApproxTokens(byteLen) };
}

export function makeToolResultBlock(
  toolUseId: string,
  content: unknown,
  isError?: boolean,
): UserTurnBlock {
  const byteLen = measureContentBytes(content);
  const block: UserTurnBlock = {
    kind: 'tool_result',
    toolUseId,
    byteLen,
    approxTokens: bytesToApproxTokens(byteLen),
  };
  if (isError === true) block.isError = true;
  return block;
}

// Measures the wire-shape byte length of a tool_result.content value: a plain
// string is measured as UTF-8; structured content is JSON-stringified first
// (matching how it'd be serialized into the request body).
export function measureContentBytes(content: unknown): number {
  if (content === undefined || content === null) return 0;
  if (typeof content === 'string') return Buffer.byteLength(content, 'utf8');
  try {
    return Buffer.byteLength(JSON.stringify(content), 'utf8');
  } catch {
    // Circular references, BigInts, etc. — fall back to a coerced string so
    // we still return a usable signal rather than zero.
    return Buffer.byteLength(String(content), 'utf8');
  }
}

export function bytesToApproxTokens(byteLen: number): number {
  if (byteLen <= 0) return 0;
  return Math.ceil(byteLen / 4);
}
