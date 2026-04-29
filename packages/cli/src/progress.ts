import type { WriteStream } from 'node:tty';

import ora, { type Ora } from 'ora';

type ProgressMode = 'spinner' | 'log' | 'silent';

export interface ProgressOptions {
  quiet?: boolean;
  stream?: NodeJS.WritableStream & Partial<Pick<WriteStream, 'isTTY'>>;
}

export interface ProgressTask {
  update(text: string): void;
  info(text: string): void;
  succeed(text?: string): void;
  fail(text?: string): void;
  stop(): void;
}

export class ProgressReporter {
  readonly mode: ProgressMode;
  readonly stream: NodeJS.WritableStream;

  constructor(opts: ProgressOptions = {}) {
    this.stream = opts.stream ?? process.stderr;
    this.mode = resolveProgressMode(opts, this.stream);
  }

  get enabled(): boolean {
    return this.mode !== 'silent';
  }

  start(text: string): ProgressTask {
    if (this.mode === 'spinner') return new OraProgressTask(text, this.stream);
    if (this.mode === 'log') return new LogProgressTask(text, this.stream);
    return silentTask;
  }

  info(text: string): void {
    if (this.mode === 'silent') return;
    this.stream.write(`[burn] ${text}\n`);
  }
}

export async function withProgress<T>(
  text: string,
  fn: (task: ProgressTask) => Promise<T>,
  opts: ProgressOptions = {},
): Promise<T> {
  const task = new ProgressReporter(opts).start(text);
  try {
    return await fn(task);
  } catch (err) {
    task.fail(`${text} failed`);
    throw err;
  } finally {
    task.stop();
  }
}

function resolveProgressMode(
  opts: ProgressOptions,
  stream: NodeJS.WritableStream & Partial<Pick<WriteStream, 'isTTY'>>,
): ProgressMode {
  if (opts.quiet === true) return 'silent';
  const forced = parseBooleanEnv(process.env['RELAYBURN_PROGRESS']);
  if (forced === false) return 'silent';
  if (forced === true) return stream.isTTY === true ? 'spinner' : 'log';
  if (process.env['CI'] === 'true') return 'silent';
  return stream.isTTY === true ? 'spinner' : 'silent';
}

function parseBooleanEnv(value: string | undefined): boolean | undefined {
  if (value === undefined) return undefined;
  switch (value.trim().toLowerCase()) {
    case '1':
    case 'true':
    case 'yes':
    case 'on':
      return true;
    case '0':
    case 'false':
    case 'no':
    case 'off':
      return false;
    default:
      return undefined;
  }
}

class OraProgressTask implements ProgressTask {
  private spinner: Ora;
  private done = false;

  constructor(text: string, stream: NodeJS.WritableStream) {
    this.spinner = ora({
      text,
      stream,
      discardStdin: false,
    }).start();
  }

  update(text: string): void {
    if (!this.done) this.spinner.text = text;
  }

  info(text: string): void {
    if (this.done) return;
    this.spinner.info(text);
    this.done = true;
  }

  succeed(text?: string): void {
    if (this.done) return;
    this.spinner.succeed(text);
    this.done = true;
  }

  fail(text?: string): void {
    if (this.done) return;
    this.spinner.fail(text);
    this.done = true;
  }

  stop(): void {
    if (this.done) return;
    this.spinner.stop();
    this.done = true;
  }
}

class LogProgressTask implements ProgressTask {
  private current: string;
  private done = false;

  constructor(text: string, private readonly stream: NodeJS.WritableStream) {
    this.current = text;
    this.stream.write(`[burn] ${text}\n`);
  }

  update(text: string): void {
    this.current = text;
  }

  info(text: string): void {
    if (this.done) return;
    this.stream.write(`[burn] ${text}\n`);
    this.done = true;
  }

  succeed(text?: string): void {
    if (this.done) return;
    this.stream.write(`[burn] ${text ?? this.current}\n`);
    this.done = true;
  }

  fail(text?: string): void {
    if (this.done) return;
    this.stream.write(`[burn] ${text ?? `${this.current} failed`}\n`);
    this.done = true;
  }

  stop(): void {
    this.done = true;
  }
}

const silentTask: ProgressTask = {
  update() {},
  info() {},
  succeed() {},
  fail() {},
  stop() {},
};
