import * as path from 'node:path';

import { describeAppliesTo } from '@relayburn/analyze';
import { ingestAll } from '@relayburn/ingest';
import {
  overhead as sdkOverhead,
  overheadTrim as sdkOverheadTrim,
  type OverheadFileKind,
  type OverheadFileSummary,
  type OverheadPerFileEntry,
  type OverheadSectionCost,
  type OverheadTrimOptions,
  type OverheadTrimResult,
} from '@relayburn/sdk';

import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { withProgress } from '../progress.js';

const HELP = `burn overhead — cost attribution for agent overhead files (CLAUDE.md, AGENTS.md, …)

Usage:
  burn overhead      [--project <path>] [--since 7d] [--kind <k>] [--json]
  burn overhead trim [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]

What it does:
  Discovers overhead files in the project (CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)
  and attributes the cached-prompt cost of each across every session that reads it.
  Different files apply to different harnesses — Claude Code doesn't pay for
  AGENTS.md, Codex/OpenCode don't pay for CLAUDE.md — so turns are filtered by
  source per file.

Flags:
  --kind <k>   narrow to a single file kind: "claude-md" or "agents-md"

Examples:
  burn overhead
  burn overhead --since 30d
  burn overhead --kind claude-md
  burn overhead trim --top 3
  burn overhead trim --json | jq '.recommendations[] | select(.projectedSavings.perSessionUsd > 0.01)'
`;

const VALID_KINDS: OverheadFileKind[] = ['claude-md', 'agents-md'];

// Injection seam for tests so they can substitute a no-op ingest. Without it,
// a test against an isolated RELAYBURN_HOME still triggers a real `ingestAll()`
// against the user's actual ~/.claude / ~/.codex / ~/.opencode session stores,
// which can take minutes and pollutes the tmp ledger with unrelated data.
export interface OverheadDeps {
  ingestAll?: typeof ingestAll;
}

export async function runOverhead(args: ParsedArgs, deps: OverheadDeps = {}): Promise<number> {
  const sub = args.positional[0];
  if (args.flags['help'] === true || sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(HELP);
    return 0;
  }
  if (sub === 'trim') return runTrim(args, deps);
  if (sub !== undefined && sub !== '') {
    process.stderr.write(`unknown overhead subcommand: ${sub}\n\n${HELP}`);
    return 1;
  }
  return runReport(args, deps);
}

interface CommonFlags {
  projectPath: string;
  /** Original user-facing `--since` string, retained for the "Cost over <since>" render label. */
  sinceLabel: string | undefined;
  /** ISO timestamp form passed to the SDK / ledger query. */
  sinceIso: string | undefined;
  kind: OverheadFileKind | undefined;
}

function parseCommonFlags(args: ParsedArgs): CommonFlags | { error: string } {
  const projectFlag = args.flags['project'];
  const projectPath =
    typeof projectFlag === 'string' && projectFlag.length > 0
      ? path.resolve(projectFlag)
      : process.cwd();

  const sinceFlag = args.flags['since'];
  const sinceLabel = typeof sinceFlag === 'string' ? sinceFlag : undefined;
  const sinceIso = sinceLabel !== undefined ? parseSinceArg(sinceLabel) : undefined;

  const kindFlag = args.flags['kind'];
  let kind: OverheadFileKind | undefined;
  if (kindFlag !== undefined && kindFlag !== true) {
    if (typeof kindFlag !== 'string' || !VALID_KINDS.includes(kindFlag as OverheadFileKind)) {
      return {
        error: `burn: invalid --kind value: ${JSON.stringify(kindFlag)} (expected one of: ${VALID_KINDS.join(', ')})\n`,
      };
    }
    kind = kindFlag as OverheadFileKind;
  }

  return { projectPath, sinceLabel, sinceIso, kind };
}

async function runReport(args: ParsedArgs, deps: OverheadDeps): Promise<number> {
  const parsed = parseCommonFlags(args);
  if ('error' in parsed) {
    process.stderr.write(parsed.error);
    return 1;
  }

  const ingest = deps.ingestAll ?? ingestAll;
  await withProgress('ingesting latest sessions', (task) =>
    ingest({
      onProgress: (message) => task.update(`ingest: ${message}`),
      onWarn: (body) => task.warn(body),
    }),
  );

  const result = await withProgress('attributing overhead cost', async (task) => {
    const opts: { project: string; since?: string; kind?: OverheadFileKind } = {
      project: parsed.projectPath,
    };
    if (parsed.sinceIso !== undefined) opts.since = parsed.sinceIso;
    if (parsed.kind !== undefined) opts.kind = parsed.kind;
    const r = await sdkOverhead(opts);
    task.succeed(`attributed overhead cost across ${formatInt(r.files.length)} file${r.files.length === 1 ? '' : 's'}`);
    return r;
  });

  if (result.files.length === 0) {
    if (parsed.kind) {
      process.stderr.write(`no ${parsed.kind} overhead files found at ${parsed.projectPath}\n`);
    } else {
      process.stderr.write(
        `no overhead files found at ${parsed.projectPath} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n`,
      );
    }
    return 1;
  }

  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(result, null, 2) + '\n');
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push(`Overhead files in ${result.project}:`);
  out.push('');

  const sinceLabel = parsed.sinceLabel ?? 'all time';

  // Pair each per-file attribution row with the parsed-file metadata (lines /
  // tokens) the renderer needs for the header line. SDK keeps them as separate
  // arrays in the result so consumers that only want one half don't pay for
  // the other; the CLI joins them by path.
  const filesByPath = new Map(result.files.map((f) => [f.path, f]));
  for (const fileAttr of result.perFile) {
    const parsedFile = filesByPath.get(fileAttr.path);
    if (!parsedFile) continue;
    renderFileBlock(parsedFile, fileAttr, sinceLabel, out);
    out.push('');
  }

  out.push(
    `Grand total (all overhead files, ${sinceLabel}): ${formatUsd(result.grandTotal)}`,
  );
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderFileBlock(
  parsedFile: OverheadFileSummary,
  fileAttr: OverheadPerFileEntry,
  sinceLabel: string,
  out: string[],
): void {
  const { attribution } = fileAttr;
  const display = `${path.basename(parsedFile.path)} (${path.relative(process.cwd(), parsedFile.path) || parsedFile.path})`;
  out.push(
    `${display} — ${formatInt(parsedFile.totalLines)} lines, ~${formatTokens(parsedFile.tokens)} tokens — applies to: ${describeAppliesTo([...parsedFile.appliesTo])}`,
  );
  if (parsedFile.tokens === 0) {
    out.push('  (empty file — no attribution)');
    return;
  }
  if (attribution.sessionCount === 0) {
    out.push('  no matching sessions in window.');
    return;
  }
  out.push(
    `  Cost per session:   avg ${formatUsd(attribution.perSessionAvg)}, p95 ${formatUsd(attribution.perSessionP95)}`,
  );
  out.push(
    `  Cost over ${sinceLabel}: ${formatUsd(attribution.totalCost)} across ${formatInt(attribution.sessionCount)} session${attribution.sessionCount === 1 ? '' : 's'}`,
  );
  out.push('  Sections ranked by cost:');
  if (attribution.sectionCosts.length === 0) {
    out.push('    (no sections)');
    return;
  }
  out.push(indent(renderSectionTable(attribution.sectionCosts), '    '));
}

async function runTrim(args: ParsedArgs, deps: OverheadDeps): Promise<number> {
  const parsed = parseCommonFlags(args);
  if ('error' in parsed) {
    process.stderr.write(parsed.error);
    return 1;
  }
  const top = parseTopN(args.flags['top']);
  const json = args.flags['json'] === true;

  const ingest = deps.ingestAll ?? ingestAll;
  await withProgress('ingesting latest sessions', (task) =>
    ingest({
      onProgress: (message) => task.update(`ingest: ${message}`),
      onWarn: (body) => task.warn(body),
    }),
  );

  const result = await withProgress('building trim recommendations', async (task) => {
    const opts: OverheadTrimOptions = { project: parsed.projectPath, top };
    if (parsed.sinceIso !== undefined) opts.since = parsed.sinceIso;
    if (parsed.sinceLabel !== undefined) opts.sinceLabel = parsed.sinceLabel;
    if (parsed.kind !== undefined) opts.kind = parsed.kind;
    const r = await sdkOverheadTrim(opts);
    task.succeed(
      `built ${formatInt(r.recommendations.length)} recommendation${r.recommendations.length === 1 ? '' : 's'}`,
    );
    return r;
  });

  if (result.summary.filesAnalyzed === 0) {
    if (parsed.kind) {
      process.stderr.write(`no ${parsed.kind} overhead files found at ${parsed.projectPath}\n`);
    } else {
      process.stderr.write(
        `no overhead files found at ${parsed.projectPath} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n`,
      );
    }
    return 1;
  }

  if (json) {
    process.stdout.write(JSON.stringify(result, null, 2) + '\n');
    return 0;
  }

  if (result.recommendations.length === 0) {
    process.stdout.write('# no trim candidates — overhead files have no headed sections\n');
    return 0;
  }

  const out: string[] = [];
  out.push('# burn overhead trim — projected savings if trimmed');
  out.push('# (recommendations only; burn never modifies your overhead files)');
  out.push('');

  const renderTrimRecsByFile = groupRecommendationsByFile(result);
  for (const [_, recs] of renderTrimRecsByFile) {
    const first = recs[0]!;
    out.push(
      `# === ${path.basename(first.file)} (applies to: ${describeAppliesTo([...first.appliesTo])}) ===`,
    );
    out.push('');
    for (const rec of recs) {
      out.push(rec.diff ?? '');
      out.push('');
    }
  }

  process.stdout.write(out.join('\n'));
  return 0;
}

function groupRecommendationsByFile(
  result: OverheadTrimResult,
): Map<string, OverheadTrimResult['recommendations']> {
  const out = new Map<string, OverheadTrimResult['recommendations']>();
  for (const rec of result.recommendations) {
    const list = out.get(rec.file);
    if (list) list.push(rec);
    else out.set(rec.file, [rec]);
  }
  return out;
}

function renderSectionTable(rows: OverheadSectionCost[]): string {
  const data: string[][] = [['lines', 'heading', 'tokens', 'cost/session', '%file']];
  for (const r of rows) {
    data.push([
      formatLineRange(r.section.startLine, r.section.endLine),
      r.section.heading,
      formatTokens(r.section.tokens),
      formatUsd(r.costPerSession),
      `${(r.tokenShare * 100).toFixed(1)}%`,
    ]);
  }
  return table(data);
}

function indent(text: string, pad: string): string {
  return text
    .split('\n')
    .map((l) => `${pad}${l}`)
    .join('\n');
}

function formatLineRange(start: number, end: number): string {
  const s = String(start).padStart(4, ' ');
  const e = String(end).padStart(4, ' ');
  return `${s}-${e}`;
}

function formatTokens(tokens: number): string {
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(1)}k`;
  return String(tokens);
}

function parseTopN(v: unknown): number {
  if (typeof v !== 'string') return 3;
  const n = Number(v);
  if (!Number.isFinite(n) || n <= 0) return 3;
  return Math.floor(n);
}
