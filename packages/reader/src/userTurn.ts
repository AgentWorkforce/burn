// Shared helpers for building UserTurnBlock entries across the Claude / Codex /
// OpenCode parsers. Parser APIs default to the cached cl100k tokenizer so
// absolute attribution has a real tokenizer-backed signal. Callers can still
// opt into the bytes/4 heuristic when they need a cheap proportional signal.

import type { UserTurnBlock } from './types.js';

export type UserTurnTokenizer = 'heuristic' | 'cl100k';

export interface UserTurnTokenCounter {
  tokenizer: UserTurnTokenizer;
  count(content: unknown, byteLen: number): number;
}

const HEURISTIC_COUNTER: UserTurnTokenCounter = {
  tokenizer: 'heuristic',
  count: (_content, byteLen) => bytesToApproxTokens(byteLen),
};

let cl100kCounter: Promise<UserTurnTokenCounter> | undefined;

export async function createUserTurnTokenCounter(
  tokenizer: UserTurnTokenizer = 'cl100k',
): Promise<UserTurnTokenCounter> {
  if (tokenizer === 'heuristic') return HEURISTIC_COUNTER;
  if (tokenizer === 'cl100k') {
    cl100kCounter ??= loadCl100kCounter();
    return cl100kCounter;
  }
  throw new Error(`Unsupported user-turn tokenizer: ${String(tokenizer)}`);
}

async function loadCl100kCounter(): Promise<UserTurnTokenCounter> {
  const { get_encoding } = await import('@dqbd/tiktoken');
  const encoder = get_encoding('cl100k_base');
  return {
    tokenizer: 'cl100k',
    count(content, byteLen) {
      if (byteLen <= 0) return 0;
      const text = stringifyMeasuredContent(content);
      if (text.length === 0) return 0;
      return encoder.encode(text).length;
    },
  };
}

export function makeTextBlock(
  text: string,
  counter: UserTurnTokenCounter = HEURISTIC_COUNTER,
): UserTurnBlock {
  const byteLen = Buffer.byteLength(text, 'utf8');
  return { kind: 'text', byteLen, approxTokens: counter.count(text, byteLen) };
}

export function makeToolResultBlock(
  toolUseId: string,
  content: unknown,
  isError?: boolean,
  counter: UserTurnTokenCounter = HEURISTIC_COUNTER,
): UserTurnBlock {
  const byteLen = measureContentBytes(content);
  const block: UserTurnBlock = {
    kind: 'tool_result',
    toolUseId,
    byteLen,
    approxTokens: counter.count(content, byteLen),
  };
  if (isError === true) block.isError = true;
  return block;
}

// Measures the wire-shape byte length of a tool_result.content value: a plain
// string is measured as UTF-8; structured content is JSON-stringified first
// (matching how it'd be serialized into the request body).
export function measureContentBytes(content: unknown): number {
  return Buffer.byteLength(stringifyMeasuredContent(content), 'utf8');
}

export function stringifyMeasuredContent(content: unknown): string {
  if (content === undefined || content === null) return '';
  if (typeof content === 'string') return content;
  try {
    return JSON.stringify(content) ?? String(content);
  } catch {
    // Circular references, BigInts, etc. — fall back to a coerced string so
    // we still return a usable signal rather than zero.
    return String(content);
  }
}

export function bytesToApproxTokens(byteLen: number): number {
  if (byteLen <= 0) return 0;
  return Math.ceil(byteLen / 4);
}
