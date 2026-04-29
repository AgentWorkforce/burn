import { readFile } from 'node:fs/promises';
import * as path from 'node:path';

import {
  attributeOverhead,
  buildTrimRecommendations,
  describeAppliesTo,
  findOverheadFiles,
  loadOverheadFile,
  loadPricing,
  renderUnifiedDiffForRecommendation,
  type TrimRecommendation,
  type OverheadAttribution,
  type OverheadFileAttribution,
  type OverheadFileKind,
  type ParsedOverheadFile,
  type SectionCost,
} from '@relayburn/analyze';
import { queryAll, type Query } from '@relayburn/ledger';
import { resolveProject } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { withProgress } from '../progress.js';

const HELP = `burn overhead — cost attribution for agent overhead files (CLAUDE.md, AGENTS.md, …)

Usage:
  burn overhead      [--project <path>] [--since 7d] [--kind <k>] [--json]
  burn overhead trim [--project <path>] [--since 7d] [--kind <k>] [--top <n>]

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
`;

const VALID_KINDS: OverheadFileKind[] = ['claude-md', 'agents-md'];

export async function runOverhead(args: ParsedArgs): Promise<number> {
  const sub = args.positional[0];
  if (args.flags['help'] === true || sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(HELP);
    return 0;
  }
  if (sub === 'trim') return runTrim(args);
  if (sub !== undefined && sub !== '') {
    process.stderr.write(`unknown overhead subcommand: ${sub}\n\n${HELP}`);
    return 1;
  }
  return runReport(args);
}

async function gatherAttribution(
  args: ParsedArgs,
  projectPath: string,
): Promise<{
  files: ParsedOverheadFile[];
  attribution: OverheadAttribution;
} | null> {
  const kindFilter = parseKindFlag(args.flags['kind']);
  if (kindFilter === 'invalid') {
    process.stderr.write(
      `burn: invalid --kind value: ${JSON.stringify(args.flags['kind'])} (expected one of: ${VALID_KINDS.join(', ')})\n`,
    );
    return null;
  }
  let found = await withProgress('finding overhead files', async (task) => {
    const files = await findOverheadFiles(projectPath);
    task.succeed(`found ${formatInt(files.length)} overhead file${files.length === 1 ? '' : 's'}`);
    return files;
  });
  if (kindFilter) found = found.filter((f) => f.kind === kindFilter);
  if (found.length === 0) {
    if (kindFilter) {
      process.stderr.write(`no ${kindFilter} overhead files found at ${projectPath}\n`);
    } else {
      process.stderr.write(
        `no overhead files found at ${projectPath} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n`,
      );
    }
    return null;
  }
  const files: ParsedOverheadFile[] = [];
  await withProgress('loading overhead files', async (task) => {
    for (const f of found) files.push(await loadOverheadFile(f));
    task.succeed(`loaded ${formatInt(files.length)} overhead file${files.length === 1 ? '' : 's'}`);
  });

  const resolved = resolveProject(projectPath);
  const q: Query = { project: resolved.projectKey ?? projectPath };
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);

  await withProgress('ingesting latest sessions', (task) =>
    ingestAll({ onProgress: (message) => task.update(`ingest: ${message}`) }),
  );
  const pricing = await withProgress('loading pricing snapshot', async (task) => {
    const loaded = await loadPricing();
    task.succeed('loaded pricing snapshot');
    return loaded;
  });
  const turns = await withProgress('reading ledger turns', async (task) => {
    const rows = await queryAll(q);
    task.succeed(`read ${formatInt(rows.length)} turn${rows.length === 1 ? '' : 's'}`);
    return rows;
  });
  const attribution = await withProgress('attributing overhead cost', async (task) => {
    const result = attributeOverhead({ files, turns, pricing });
    task.succeed('attributed overhead cost');
    return result;
  });
  return { files, attribution };
}

async function runReport(args: ParsedArgs): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath);
  if (!data) return 1;

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          project: projectPath,
          files: data.files.map(({ file, parsed }) => ({
            kind: file.kind,
            path: file.path,
            appliesTo: file.appliesTo,
            totalLines: parsed.totalLines,
            bytes: parsed.bytes,
            tokens: parsed.tokens,
            sections: parsed.sections,
            groupingLevel: parsed.groupingLevel,
          })),
          perFile: data.attribution.perFile.map((p) => ({
            path: p.file.path,
            kind: p.file.kind,
            appliesTo: p.file.appliesTo,
            attribution: p.attribution,
          })),
          grandTotal: data.attribution.grandTotal,
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push(`Overhead files in ${projectPath}:`);
  out.push('');

  const sinceLabel = typeof args.flags['since'] === 'string' ? args.flags['since'] : 'all time';

  for (const fileAttr of data.attribution.perFile) {
    renderFileBlock(fileAttr, sinceLabel, out);
    out.push('');
  }

  out.push(`Grand total (all overhead files, ${sinceLabel}): ${formatUsd(data.attribution.grandTotal)}`);
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderFileBlock(
  fileAttr: OverheadFileAttribution,
  sinceLabel: string,
  out: string[],
): void {
  const { file, parsed, attribution } = fileAttr;
  const display = `${path.basename(file.path)} (${path.relative(process.cwd(), file.path) || file.path})`;
  out.push(
    `${display} — ${formatInt(parsed.totalLines)} lines, ~${formatTokens(parsed.tokens)} tokens — applies to: ${describeAppliesTo(file.appliesTo)}`,
  );
  if (parsed.tokens === 0) {
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

async function runTrim(args: ParsedArgs): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath);
  if (!data) return 1;

  const topPerFile = parseTopN(args.flags['top']);

  const out: string[] = [];
  out.push('# burn overhead trim — projected savings if trimmed');
  out.push('# (recommendations only; burn never modifies your overhead files)');
  out.push('');

  const textCache = new Map<string, string>();
  let emitted = 0;
  for (const fileAttr of data.attribution.perFile) {
    const recs = buildTrimRecommendations(fileAttr.attribution, topPerFile);
    if (recs.length === 0) continue;
    out.push(`# === ${path.basename(fileAttr.file.path)} (applies to: ${describeAppliesTo(fileAttr.file.appliesTo)}) ===`);
    out.push('');
    let text = textCache.get(fileAttr.file.path);
    if (text === undefined) {
      text = await readFile(fileAttr.file.path, 'utf8');
      textCache.set(fileAttr.file.path, text);
    }
    for (const rec of recs) {
      out.push(
        renderUnifiedDiffForRecommendation(fileAttr.file.path, text, rec as TrimRecommendation, projectPath),
      );
      out.push('');
    }
    emitted++;
  }
  if (emitted === 0) {
    process.stdout.write('# no trim candidates — overhead files have no headed sections\n');
    return 0;
  }
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderSectionTable(rows: SectionCost[]): string {
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

function parseKindFlag(v: unknown): OverheadFileKind | null | 'invalid' {
  if (v === undefined || v === true) return null;
  if (typeof v !== 'string') return 'invalid';
  if (v === 'claude-md' || v === 'agents-md') return v;
  return 'invalid';
}
