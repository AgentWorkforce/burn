import { Buffer } from 'node:buffer';

export interface SseEvent {
  event?: string;
  id?: string;
  data: string;
}

export interface OpencodeEventStreamReport {
  events: number;
  wakeups: number;
}

export interface FetchResponseLike {
  ok: boolean;
  status: number;
  statusText: string;
  body: ReadableStream<Uint8Array> | null;
}

export type FetchLike = (
  url: string,
  init: { headers: Record<string, string>; signal?: AbortSignal },
) => Promise<FetchResponseLike>;

export interface ConsumeOpencodeEventStreamOptions {
  baseUrl?: string;
  global?: boolean;
  fetchImpl?: FetchLike;
  signal?: AbortSignal;
  env?: NodeJS.ProcessEnv;
  onEvent?: (event: SseEvent, payload: unknown) => void;
  onIngestHint?: (payload: unknown) => void;
}

export interface OpencodeEventStreamController {
  stop(): Promise<void>;
}

export interface StartOpencodeEventStreamOptions extends ConsumeOpencodeEventStreamOptions {
  onError?: (err: unknown) => void;
  onOpen?: (url: string) => void;
}

const DEFAULT_OPENCODE_BASE_URL = 'http://127.0.0.1:4096';

export function resolveOpencodeEventUrl(
  baseUrl: string | undefined,
  opts: { global?: boolean; env?: NodeJS.ProcessEnv } = {},
): string {
  const env = opts.env ?? process.env;
  const raw = baseUrl ?? env['OPENCODE_SERVER_URL'] ?? DEFAULT_OPENCODE_BASE_URL;
  const url = new URL(raw);
  if (url.pathname === '' || url.pathname === '/') {
    url.pathname = opts.global === true ? '/global/event' : '/event';
  }
  return url.toString();
}

export function buildOpencodeEventHeaders(
  env: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const headers: Record<string, string> = { Accept: 'text/event-stream' };
  const password = env['OPENCODE_SERVER_PASSWORD'];
  if (password !== undefined && password.length > 0) {
    const username = env['OPENCODE_SERVER_USERNAME'] || 'opencode';
    const token = Buffer.from(`${username}:${password}`, 'utf8').toString('base64');
    headers['Authorization'] = `Basic ${token}`;
  }
  return headers;
}

export function splitSseFrames(buffer: string): { frames: string[]; rest: string } {
  const frames: string[] = [];
  const re = /\r?\n\r?\n/g;
  let start = 0;
  let match: RegExpExecArray | null;
  while ((match = re.exec(buffer)) !== null) {
    frames.push(buffer.slice(start, match.index));
    start = match.index + match[0].length;
  }
  return { frames, rest: buffer.slice(start) };
}

export function parseSseEvent(block: string): SseEvent | null {
  let event: string | undefined;
  let id: string | undefined;
  const data: string[] = [];

  for (const rawLine of block.split(/\r?\n/)) {
    if (rawLine.length === 0 || rawLine.startsWith(':')) continue;
    const colon = rawLine.indexOf(':');
    const field = colon >= 0 ? rawLine.slice(0, colon) : rawLine;
    let value = colon >= 0 ? rawLine.slice(colon + 1) : '';
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') event = value;
    else if (field === 'id') id = value;
    else if (field === 'data') data.push(value);
  }

  if (data.length === 0) return null;
  const parsed: SseEvent = { data: data.join('\n') };
  if (event !== undefined) parsed.event = event;
  if (id !== undefined) parsed.id = id;
  return parsed;
}

export function isOpencodeIngestHint(payload: unknown): boolean {
  const type = opencodeEventType(payload);
  if (type === undefined) return false;
  return (
    type === 'message.updated' ||
    type === 'message.part.updated' ||
    type === 'message.part.removed' ||
    type === 'session.created' ||
    type === 'session.updated' ||
    type === 'session.deleted' ||
    type === 'session.idle' ||
    type === 'session.compacted' ||
    type === 'session.status' ||
    type === 'message.updated.1' ||
    type === 'message.part.updated.1' ||
    type === 'message.part.removed.1' ||
    type === 'session.created.1' ||
    type === 'session.updated.1' ||
    type === 'session.deleted.1'
  );
}

export async function consumeOpencodeEventStream(
  opts: ConsumeOpencodeEventStreamOptions,
): Promise<OpencodeEventStreamReport> {
  const fetchImpl = opts.fetchImpl ?? globalThis.fetch;
  const env = opts.env ?? process.env;
  const url = resolveOpencodeEventUrl(opts.baseUrl, resolveUrlOptions(opts.global, env));
  const response = await fetchImpl(url, {
    headers: buildOpencodeEventHeaders(env),
    ...(opts.signal !== undefined ? { signal: opts.signal } : {}),
  });
  if (!response.ok) {
    throw new Error(
      `OpenCode event stream ${url} returned HTTP ${response.status} ${response.statusText}`,
    );
  }
  if (response.body === null) {
    throw new Error(`OpenCode event stream ${url} returned no response body`);
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let events = 0;
  let wakeups = 0;

  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const split = splitSseFrames(buffer);
    buffer = split.rest;
    for (const frame of split.frames) {
      const parsed = parseSseEvent(frame);
      if (!parsed) continue;
      const payload = parseEventPayload(parsed.data);
      events++;
      opts.onEvent?.(parsed, payload);
      if (isOpencodeIngestHint(payload)) {
        wakeups++;
        opts.onIngestHint?.(payload);
      }
    }
  }

  buffer += decoder.decode();
  const trailing = buffer.trim().length > 0 ? parseSseEvent(buffer) : null;
  if (trailing) {
    const payload = parseEventPayload(trailing.data);
    events++;
    opts.onEvent?.(trailing, payload);
    if (isOpencodeIngestHint(payload)) {
      wakeups++;
      opts.onIngestHint?.(payload);
    }
  }

  return { events, wakeups };
}

export function startOpencodeEventStream(
  opts: StartOpencodeEventStreamOptions,
): OpencodeEventStreamController {
  const controller = new AbortController();
  let stopped = false;
  const url = resolveOpencodeEventUrl(
    opts.baseUrl,
    resolveUrlOptions(opts.global, opts.env),
  );
  opts.onOpen?.(url);
  const done = consumeOpencodeEventStream({
    ...opts,
    baseUrl: url,
    signal: controller.signal,
  })
    .then(() => {
      if (!stopped) opts.onError?.(new Error('connection closed'));
    })
    .catch((err: unknown) => {
      if (isAbortError(err)) return;
      opts.onError?.(err);
    });

  return {
    async stop() {
      stopped = true;
      controller.abort();
      await done;
    },
  };
}

function parseEventPayload(data: string): unknown {
  try {
    return JSON.parse(data) as unknown;
  } catch {
    return data;
  }
}

function resolveUrlOptions(
  global: boolean | undefined,
  env: NodeJS.ProcessEnv | undefined,
): { global?: boolean; env?: NodeJS.ProcessEnv } {
  const out: { global?: boolean; env?: NodeJS.ProcessEnv } = {};
  if (global !== undefined) out.global = global;
  if (env !== undefined) out.env = env;
  return out;
}

function opencodeEventType(payload: unknown): string | undefined {
  if (!payload || typeof payload !== 'object') return undefined;
  const rec = payload as { type?: unknown; payload?: unknown };
  if (typeof rec.type === 'string') return rec.type;
  if (rec.payload && typeof rec.payload === 'object') {
    const nested = rec.payload as { type?: unknown };
    if (typeof nested.type === 'string') return nested.type;
  }
  return undefined;
}

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === 'AbortError';
}
