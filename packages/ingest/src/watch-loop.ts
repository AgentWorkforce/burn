import { ingestAll, type IngestOptions, type IngestReport } from './ingest.js';

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

export async function runIngestTick(opts: IngestOptions = {}): Promise<IngestReport> {
  return ingestAll(opts);
}

export function startWatchLoop(opts: StartWatchLoopOptions = {}): WatchController {
  const intervalMs = opts.intervalMs ?? 1000;
  const ingest = opts.ingest ?? runIngestTick;
  const onError = opts.onError ?? ((err: unknown) => {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] ingest: ${msg}\n`);
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
