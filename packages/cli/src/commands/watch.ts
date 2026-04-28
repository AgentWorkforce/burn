import type { ParsedArgs } from '../args.js';
import { formatInt } from '../format.js';
import { ingestAll, type IngestReport } from '../ingest.js';
import { startOpencodeEventStream } from '../opencode-stream.js';

const WATCH_HELP = `burn watch — foreground incremental ingest

Usage:
  burn watch [--interval <ms>] [--once] [--opencode-stream] [--opencode-url <url>]
  burn watch --daemon

Continuously scans Claude Code, Codex, and OpenCode session stores and ingests
new committed turns using the same cursor + dedup paths as summary/compare.

--opencode-stream also subscribes to OpenCode's local SSE endpoint and wakes
the same ingest loop on session/message events. Polling remains enabled as the
fallback path. The default OpenCode URL is http://127.0.0.1:4096 or
OPENCODE_SERVER_URL. OPENCODE_SERVER_USERNAME/PASSWORD are used for Basic auth
when present.

--daemon is not supported yet; run burn watch in the foreground.
`;

export interface WatchController {
  tick(): Promise<void>;
  stop(): Promise<void>;
}

export interface StartWatchLoopOptions {
  intervalMs?: number;
  immediate?: boolean;
  ingest?: () => Promise<IngestReport>;
  onReport?: (report: IngestReport) => void;
  onError?: (err: unknown) => void;
}

export async function runWatch(args: ParsedArgs): Promise<number> {
  if (args.positional[0] === 'help' || args.flags['help'] === true) {
    process.stdout.write(WATCH_HELP);
    return 0;
  }
  if (args.flags['daemon'] === true) {
    process.stderr.write(`burn: watch --daemon is not supported yet; run burn watch in the foreground.\n`);
    return 2;
  }
  if (typeof args.flags['opencode-stream'] === 'string') {
    process.stderr.write(`burn: watch --opencode-stream does not take a value\n`);
    return 2;
  }

  const intervalMs = parseIntervalMs(args.flags['interval']);
  if (intervalMs === null) {
    process.stderr.write(`burn: watch --interval must be a positive integer in milliseconds\n`);
    return 2;
  }

  if (args.flags['once'] === true) {
    const report = await runWatchTick();
    process.stdout.write(renderWatchReport(report));
    return 0;
  }

  process.stderr.write(`[burn] watch: foreground ingest every ${intervalMs}ms; Ctrl-C to stop\n`);
  const controller = startWatchLoop({
    intervalMs,
    immediate: true,
    onReport(report) {
      if (report.appendedTurns > 0) process.stderr.write(renderWatchReport(report));
    },
    onError(err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] watch: ${msg}\n`);
    },
  });
  const stream =
    args.flags['opencode-stream'] === true
      ? startOpencodeEventStream({
          ...opencodeStreamFlags(args),
          onOpen(url) {
            process.stderr.write(`[burn] watch: OpenCode event stream ${url}\n`);
          },
          onIngestHint() {
            void controller.tick();
          },
          onError(err) {
            const msg = err instanceof Error ? err.message : String(err);
            process.stderr.write(
              `[burn] watch: OpenCode event stream unavailable (${msg}); polling continues\n`,
            );
          },
        })
      : undefined;

  await waitForStopSignal();
  if (stream) await stream.stop();
  await controller.stop();
  return 0;
}

export async function runWatchTick(): Promise<IngestReport> {
  return ingestAll();
}

export function startWatchLoop(opts: StartWatchLoopOptions = {}): WatchController {
  const intervalMs = opts.intervalMs ?? 1000;
  const ingest = opts.ingest ?? runWatchTick;
  const onError = opts.onError ?? ((err: unknown) => {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] watch: ${msg}\n`);
  });
  let stopped = false;
  let running: Promise<void> | undefined;

  async function tick(): Promise<void> {
    if (running) return running;
    running = (async () => {
      try {
        const report = await ingest();
        opts.onReport?.(report);
      } catch (err) {
        onError(err);
      } finally {
        running = undefined;
      }
    })();
    return running;
  }

  const timer = setInterval(() => {
    if (!stopped) void tick();
  }, intervalMs);
  if (opts.immediate !== false) void tick();

  return {
    tick,
    async stop() {
      stopped = true;
      clearInterval(timer);
      if (running) await running;
    },
  };
}

function renderWatchReport(report: IngestReport): string {
  return (
    `[burn] watch: ingested ${formatInt(report.ingestedSessions)} session` +
    `${report.ingestedSessions === 1 ? '' : 's'} ` +
    `(+${formatInt(report.appendedTurns)} turn${report.appendedTurns === 1 ? '' : 's'})\n`
  );
}

function parseIntervalMs(raw: string | true | undefined): number | null {
  if (raw === undefined || raw === true) return 1000;
  const n = Number(raw);
  if (!Number.isInteger(n) || n <= 0) return null;
  return n;
}

function stringFlag(raw: string | true | undefined): string | undefined {
  return typeof raw === 'string' ? raw : undefined;
}

function opencodeStreamFlags(args: ParsedArgs): { baseUrl?: string; global?: boolean } {
  const out: { baseUrl?: string; global?: boolean } = {};
  const baseUrl = stringFlag(args.flags['opencode-url']);
  if (baseUrl !== undefined) out.baseUrl = baseUrl;
  if (args.flags['opencode-global'] === true) out.global = true;
  return out;
}

async function waitForStopSignal(): Promise<void> {
  await new Promise<void>((resolve) => {
    const done = (): void => {
      process.off('SIGINT', done);
      process.off('SIGTERM', done);
      resolve();
    };
    process.once('SIGINT', done);
    process.once('SIGTERM', done);
  });
}
