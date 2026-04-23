import { readFile } from 'node:fs/promises';
import * as path from 'node:path';

import {
  attributeClaudeMd,
  buildAdviseRecommendations,
  findClaudeMdFiles,
  loadClaudeMdFile,
  loadPricing,
  renderUnifiedDiffForRecommendation,
  type AdviseRecommendation,
  type ClaudeMdAttributionResult,
  type ParsedClaudeMd,
  type SectionCost,
} from '@relayburn/analyze';
import { queryAll, type Query } from '@relayburn/ledger';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

const HELP = `burn claude-md — CLAUDE.md hot-path cost attribution

Usage:
  burn claude-md         [--project <path>] [--since 7d] [--json]
  burn claude-md advise  [--project <path>] [--since 7d] [--top <n>]

Examples:
  burn claude-md
  burn claude-md --since 30d
  burn claude-md advise --top 3
`;

export async function runClaudeMd(args: ParsedArgs): Promise<number> {
  const sub = args.positional[0];
  if (sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(HELP);
    return 0;
  }
  if (sub === 'advise') {
    return runAdvise(args);
  }
  if (sub !== undefined && sub !== '') {
    process.stderr.write(`unknown claude-md subcommand: ${sub}\n\n${HELP}`);
    return 1;
  }
  return runReport(args);
}

async function loadParsedFiles(projectPath: string): Promise<ParsedClaudeMd[]> {
  const paths = await findClaudeMdFiles(projectPath);
  const parsed: ParsedClaudeMd[] = [];
  for (const p of paths) parsed.push(await loadClaudeMdFile(p));
  return parsed;
}

async function gatherAttribution(
  args: ParsedArgs,
  projectPath: string,
): Promise<{
  files: ParsedClaudeMd[];
  attribution: ClaudeMdAttributionResult;
} | null> {
  const files = await loadParsedFiles(projectPath);
  if (files.length === 0) {
    process.stderr.write(`no CLAUDE.md found at ${projectPath} or ${path.join(projectPath, '.claude')}\n`);
    return null;
  }

  const q: Query = { project: projectPath };
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);
  const attribution = attributeClaudeMd({ files, turns, pricing });
  return { files, attribution };
}

async function runReport(args: ParsedArgs): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath);
  if (!data) return 1;

  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify({
      project: projectPath,
      files: data.files.map(({ path, totalLines, bytes, tokens, sections, groupingLevel }) => ({
        path, totalLines, bytes, tokens, sections, groupingLevel,
      })),
      attribution: data.attribution,
    }, null, 2) + '\n');
    return 0;
  }

  const out: string[] = [];
  out.push('');
  for (const f of data.files) {
    out.push(
      `CLAUDE.md at ${f.path} (${formatInt(f.totalLines)} lines, ~${formatTokens(f.tokens)} tokens)`,
    );
  }
  if (data.attribution.totalTokens === 0) {
    out.push('CLAUDE.md is empty — no attribution.');
    process.stdout.write(out.join('\n') + '\n');
    return 0;
  }
  out.push('');
  out.push(
    `Cost per session:   avg ${formatUsd(data.attribution.perSessionAvg)}, p95 ${formatUsd(data.attribution.perSessionP95)}`,
  );
  const sinceLabel = typeof args.flags['since'] === 'string' ? args.flags['since'] : 'all time';
  out.push(
    `Cost over ${sinceLabel}: ${formatUsd(data.attribution.totalCost)} across ${formatInt(data.attribution.sessionCount)} session${data.attribution.sessionCount === 1 ? '' : 's'}`,
  );
  out.push('');
  out.push('Sections ranked by cost:');
  if (data.attribution.sectionCosts.length === 0) {
    out.push('  (no sections found)');
  } else {
    out.push(renderSectionTable(data.attribution.sectionCosts));
  }
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

async function runAdvise(args: ParsedArgs): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath);
  if (!data) return 1;

  const topN = parseTopN(args.flags['top']);
  const recs = buildAdviseRecommendations(data.attribution, topN);
  if (recs.length === 0) {
    process.stdout.write('# no trim candidates — CLAUDE.md has no headed sections\n');
    return 0;
  }

  const out: string[] = [];
  out.push('# burn claude-md advise — projected savings if trimmed');
  out.push('# (recommendations only; burn never modifies your CLAUDE.md)');
  out.push('');

  // Read the file text once per filePath for diff generation.
  const textCache = new Map<string, string>();
  const groupedByPath = groupBy(recs, (r: AdviseRecommendation) => r.filePath);
  for (const [filePath, fileRecs] of groupedByPath) {
    let text = textCache.get(filePath);
    if (text === undefined) {
      text = await readFile(filePath, 'utf8');
      textCache.set(filePath, text);
    }
    for (const rec of fileRecs) {
      out.push(renderUnifiedDiffForRecommendation(filePath, text, rec));
      out.push('');
    }
  }
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderSectionTable(rows: SectionCost[]): string {
  const data: string[][] = [
    ['lines', 'heading', 'tokens', 'cost/session', '%file'],
  ];
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

function formatLineRange(start: number, end: number): string {
  const s = String(start).padStart(4, ' ');
  const e = String(end).padStart(4, ' ');
  return `${s}-${e}`;
}

function formatTokens(tokens: number): string {
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(1)}k`;
  return String(tokens);
}

function resolveProjectPath(args: ParsedArgs): string {
  const flag = args.flags['project'];
  if (typeof flag === 'string' && flag.length > 0) return path.resolve(flag);
  return process.cwd();
}

function parseTopN(v: unknown): number {
  if (typeof v !== 'string') return 3;
  const n = Number(v);
  if (!Number.isFinite(n) || n <= 0) return 3;
  return Math.floor(n);
}

function groupBy<T, K>(items: T[], key: (t: T) => K): Map<K, T[]> {
  const out = new Map<K, T[]>();
  for (const item of items) {
    const k = key(item);
    let bucket = out.get(k);
    if (!bucket) {
      bucket = [];
      out.set(k, bucket);
    }
    bucket.push(item);
  }
  return out;
}
