import { stat } from 'node:fs/promises';

import { parseClaudeSessionIncremental } from '@relayburn/reader';
import {
  appendCompactions,
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  loadConfig,
  loadCursors,
  saveCursors,
} from '@relayburn/ledger';
import type { ClaudeCursor } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';

const INGEST_HELP = `burn ingest — hook-driven ingest from an agent harness

Usage:
  burn ingest --runtime claude [--quiet]

Reads a hook payload JSON from stdin and incrementally ingests the session
transcript it references. Safe to call from every Claude Code hook
(PreToolUse, PostToolUse, UserPromptSubmit, SubagentStop, SessionEnd) — the
ledger's cursor + dedup index keep re-invocations idempotent.
`;

interface ClaudeHookPayload {
  session_id?: string;
  transcript_path?: string;
  hook_event_name?: string;
}

export async function runIngest(args: ParsedArgs): Promise<number> {
  const runtime = typeof args.flags['runtime'] === 'string' ? args.flags['runtime'] : undefined;
  const quiet = args.flags['quiet'] === true;
  if (args.positional[0] === 'help' || args.flags['help'] === true) {
    process.stdout.write(INGEST_HELP);
    return 0;
  }
  if (!runtime) {
    process.stderr.write(`burn: ingest requires --runtime <claude>\n\n${INGEST_HELP}`);
    return 2;
  }
  if (runtime !== 'claude') {
    process.stderr.write(`burn: unsupported runtime: ${runtime}\n\n${INGEST_HELP}`);
    return 2;
  }

  const raw = await readStdin();
  return ingestClaudeHookPayload(raw, { quiet });
}

export async function ingestClaudeHookPayload(
  raw: string,
  opts: { quiet: boolean },
): Promise<number> {
  if (!raw.trim()) {
    if (!opts.quiet) process.stderr.write(`[burn] ingest: empty stdin payload, nothing to do\n`);
    return 0;
  }
  let payload: ClaudeHookPayload;
  try {
    payload = JSON.parse(raw) as ClaudeHookPayload;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] ingest: invalid JSON payload: ${msg}\n`);
    return 1;
  }
  const sessionId = payload.session_id;
  const transcriptPath = payload.transcript_path;
  if (!sessionId || !transcriptPath) {
    if (!opts.quiet) {
      process.stderr.write(
        `[burn] ingest: payload missing session_id or transcript_path; ignoring\n`,
      );
    }
    return 0;
  }
  try {
    await ingestClaudeTranscript(transcriptPath, opts);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Never propagate failure back to Claude Code — a non-zero exit from a
    // hook command can block the tool call. Log and move on.
    process.stderr.write(`[burn] ingest: ${msg}\n`);
  }
  return 0;
}

async function ingestClaudeTranscript(
  file: string,
  opts: { quiet: boolean },
): Promise<void> {
  let st: Awaited<ReturnType<typeof stat>>;
  try {
    st = await stat(file);
  } catch {
    if (!opts.quiet) process.stderr.write(`[burn] ingest: no transcript at ${file}\n`);
    return;
  }
  if (!st.isFile()) return;

  const cfg = await loadConfig();
  const cursors = await loadCursors();
  const prior = cursors[file];
  const priorClaude = prior?.kind === 'claude' ? prior : undefined;
  const rotated =
    !priorClaude ||
    priorClaude.inode !== st.ino ||
    st.mtimeMs < priorClaude.mtimeMs ||
    st.size < priorClaude.offsetBytes;
  const startOffset = rotated ? 0 : priorClaude.offsetBytes;

  if (!rotated && startOffset >= st.size) {
    priorClaude.mtimeMs = st.mtimeMs;
    await saveCursors(cursors);
    return;
  }

  const parseOpts: Parameters<typeof parseClaudeSessionIncremental>[1] = {
    startOffset,
    sessionPath: file,
    contentMode: cfg.content.store,
  };
  const priorUserText = rotated ? undefined : priorClaude?.lastUserText;
  if (priorUserText) parseOpts.lastUserText = priorUserText;

  const {
    turns,
    content,
    events,
    relationships,
    toolResultEvents,
    endOffset,
    lastUserText,
  } = await parseClaudeSessionIncremental(file, parseOpts);

  if (turns.length > 0) await appendTurns(turns);
  if (content.length > 0) await appendContent(content);
  if (events.length > 0) await appendCompactions(events);
  if (relationships.length > 0) await appendRelationships(relationships);
  if (toolResultEvents.length > 0) await appendToolResultEvents(toolResultEvents);

  const next: ClaudeCursor = {
    kind: 'claude',
    inode: st.ino,
    offsetBytes: endOffset,
    mtimeMs: st.mtimeMs,
  };
  if (lastUserText) next.lastUserText = lastUserText;
  cursors[file] = next;
  await saveCursors(cursors);

  if (!opts.quiet && turns.length > 0) {
    process.stderr.write(`[burn] ingested ${turns.length} turns from ${file}\n`);
  }
}

async function readStdin(): Promise<string> {
  if (process.stdin.isTTY) return '';
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) {
    chunks.push(typeof chunk === 'string' ? Buffer.from(chunk) : chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}
