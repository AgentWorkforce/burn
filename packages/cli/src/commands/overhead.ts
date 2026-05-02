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
import { resolveProject, type TurnRecord } from '@relayburn/reader';

import { ingestAll } from '@relayburn/ingest';
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

export interface OverheadDeps {
  ingestAll?: typeof ingestAll;
  queryAll?: (q: Query) => Promise<TurnRecord[]>;
  loadPricing?: typeof loadPricing;
}

interface TrimJsonRecommendation {
  file: string;
  kind: OverheadFileKind;
  appliesTo: string[];
  section: {
    heading: string;
    startLine: number;
    endLine: number;
    tokens: number;
  };
  projectedSavings: {
    perSessionUsd: number;
    acrossWindowUsd: number;
    tokens: number;
    tokenShare: number;
  };
  diff: string;
}

interface TrimJsonPayload {
  project: string;
  since: string;
  recommendations: TrimJsonRecommendation[];
  summary: {
    filesAnalyzed: number;
    filesWithRecommendations: number;
    totalRecommendations: number;
    totalProjectedSavingsPerSession: number;
    totalProjectedSavingsAcrossWindow: number;
  };
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

async function gatherAttribution(
  args: ParsedArgs,
  projectPath: string,
  deps: OverheadDeps,
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

  const ingest = deps.ingestAll ?? ingestAll;
  await withProgress('ingesting latest sessions', (task) =>
    ingest({ onProgress: (message) => task.update(`ingest: ${message}`) }),
  );
  const pricingLoader = deps.loadPricing ?? loadPricing;
  const pricing = await withProgress('loading pricing snapshot', async (task) => {
    const loaded = await pricingLoader();
    task.succeed('loaded pricing snapshot');
    return loaded;
  });
  const turnQuery = deps.queryAll ?? queryAll;
  const turns = await withProgress('reading ledger turns', async (task) => {
    const rows = await turnQuery(q);
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

async function runReport(args: ParsedArgs, deps: OverheadDeps): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath, deps);
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

async function runTrim(args: ParsedArgs, deps: OverheadDeps): Promise<number> {
  const projectPath = resolveProjectPath(args);
  const data = await gatherAttribution(args, projectPath, deps);
  if (!data) return 1;

  const topPerFile = parseTopN(args.flags['top']);
  const json = args.flags['json'] === true;

  if (json) {
    const payload = await buildTrimJsonPayload(args, projectPath, data, topPerFile);
    process.stdout.write(JSON.stringify(payload, null, 2) + '\n');
    return 0;
  }

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

async function buildTrimJsonPayload(
  args: ParsedArgs,
  projectPath: string,
  data: {
    files: ParsedOverheadFile[];
    attribution: OverheadAttribution;
  },
  topPerFile: number,
): Promise<TrimJsonPayload> {
  const textCache = new Map<string, string>();
  const recommendations: TrimJsonRecommendation[] = [];
  let filesWithRecommendations = 0;

  for (const fileAttr of data.attribution.perFile) {
    const recs = buildTrimRecommendations(fileAttr.attribution, topPerFile);
    if (recs.length === 0) continue;
    filesWithRecommendations++;
    let text = textCache.get(fileAttr.file.path);
    if (text === undefined) {
      text = await readFile(fileAttr.file.path, 'utf8');
      textCache.set(fileAttr.file.path, text);
    }
    for (const rec of recs) {
      recommendations.push({
        file: toProjectRelativePath(fileAttr.file.path, projectPath),
        kind: fileAttr.file.kind,
        appliesTo: fileAttr.file.appliesTo,
        section: {
          heading: rec.section.heading,
          startLine: rec.section.startLine,
          endLine: rec.section.endLine,
          tokens: rec.section.tokens,
        },
        projectedSavings: {
          perSessionUsd: rec.projectedSavingsPerSession,
          acrossWindowUsd: rec.projectedSavingsAcrossWindow,
          tokens: rec.section.tokens,
          tokenShare: rec.tokenShare,
        },
        diff: renderUnifiedDiffForRecommendation(fileAttr.file.path, text, rec, projectPath),
      });
    }
  }

  return {
    project: projectPath,
    since: typeof args.flags['since'] === 'string' ? args.flags['since'] : 'all time',
    recommendations,
    summary: {
      filesAnalyzed: data.files.length,
      filesWithRecommendations,
      totalRecommendations: recommendations.length,
      totalProjectedSavingsPerSession: recommendations.reduce(
        (sum, rec) => sum + rec.projectedSavings.perSessionUsd,
        0,
      ),
      totalProjectedSavingsAcrossWindow: recommendations.reduce(
        (sum, rec) => sum + rec.projectedSavings.acrossWindowUsd,
        0,
      ),
    },
  };
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

function toProjectRelativePath(filePath: string, projectPath: string): string {
  const rel = path.relative(projectPath, filePath);
  const display = rel && !rel.startsWith('..') ? rel : filePath;
  return display.split(path.sep).join('/');
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
