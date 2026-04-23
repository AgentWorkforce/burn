import { stat, readFile } from 'node:fs/promises';
import * as path from 'node:path';

import type { TurnRecord } from '@relayburn/reader';

import type { ModelCost, PricingTable } from './pricing.js';

const PER_MILLION = 1_000_000;
const CHARS_PER_TOKEN = 4;

export interface MarkdownSection {
  heading: string;
  level: number; // 0 for preamble, 1-6 for # through ######
  startLine: number; // 1-indexed
  endLine: number; // 1-indexed inclusive
  bytes: number;
  tokens: number;
}

export interface ParsedClaudeMd {
  path: string;
  totalLines: number;
  bytes: number;
  tokens: number;
  // Top-level sections at the inferred grouping level (H2 if any exist,
  // otherwise H1, otherwise just preamble).
  sections: MarkdownSection[];
  groupingLevel: number; // 1 or 2; 0 if no headings
}

export interface SessionClaudeMdCost {
  sessionId: string;
  cost: number;
  ridingTurns: number;
  totalTurns: number;
  model: string;
}

export interface SectionCost {
  filePath: string;
  section: MarkdownSection;
  tokenShare: number; // section.tokens / total tokens across all files
  costPerSession: number; // average per-session cost attributed to this section
  totalCost: number; // sum across all sessions
}

export interface ClaudeMdAttributionResult {
  totalTokens: number;
  totalCost: number;
  sessionCosts: SessionClaudeMdCost[];
  sectionCosts: SectionCost[];
  perSessionAvg: number;
  perSessionP95: number;
  sessionCount: number;
}

export interface AttributeClaudeMdInput {
  files: ParsedClaudeMd[];
  turns: TurnRecord[];
  pricing: PricingTable;
}

export async function findClaudeMdFiles(projectPath: string): Promise<string[]> {
  const candidates = [
    path.join(projectPath, 'CLAUDE.md'),
    path.join(projectPath, '.claude', 'CLAUDE.md'),
  ];
  const found: string[] = [];
  for (const c of candidates) {
    try {
      const st = await stat(c);
      if (st.isFile()) found.push(c);
    } catch {
      // ignore missing
    }
  }
  return found;
}

export async function loadClaudeMdFile(filePath: string): Promise<ParsedClaudeMd> {
  const text = await readFile(filePath, 'utf8');
  return parseClaudeMd(filePath, text);
}

export function parseClaudeMd(filePath: string, text: string): ParsedClaudeMd {
  const lines = text.split('\n');
  const totalLines = lines.length;
  const bytes = Buffer.byteLength(text, 'utf8');
  const tokens = estimateTokens(text);

  const headings = findHeadings(lines);
  // Choose the grouping level:
  //   - If any H2 exists: group at H2 (per the issue's example output).
  //   - Else if any H1 exists: group at H1.
  //   - Else: no headings at all — single preamble section.
  let groupingLevel = 0;
  if (headings.some((h) => h.level === 2)) groupingLevel = 2;
  else if (headings.some((h) => h.level === 1)) groupingLevel = 1;

  const sections: MarkdownSection[] = [];
  if (groupingLevel === 0) {
    if (totalLines > 0 && bytes > 0) {
      sections.push({
        heading: '(preamble)',
        level: 0,
        startLine: 1,
        endLine: totalLines,
        bytes,
        tokens,
      });
    }
    return { path: filePath, totalLines, bytes, tokens, sections, groupingLevel };
  }

  // Only headings AT the grouping level become top-level sections. Higher-
  // level headings (e.g. an H1 doc title above a series of H2 sections) get
  // folded into the preamble, matching the standard CLAUDE.md shape where
  // the H1 is a document title and H2s are the meaningful sections.
  const groupHeadings = headings.filter((h) => h.level === groupingLevel);
  // Preamble: content before the first grouping heading.
  const firstStart = groupHeadings[0]?.line ?? totalLines + 1;
  if (firstStart > 1) {
    const preambleLines = lines.slice(0, firstStart - 1);
    const preambleBytes = Buffer.byteLength(preambleLines.join('\n'), 'utf8');
    if (preambleBytes > 0) {
      sections.push({
        heading: '(preamble)',
        level: 0,
        startLine: 1,
        endLine: firstStart - 1,
        bytes: preambleBytes,
        tokens: Math.max(0, Math.ceil(preambleBytes / CHARS_PER_TOKEN)),
      });
    }
  }

  for (let i = 0; i < groupHeadings.length; i++) {
    const h = groupHeadings[i]!;
    const next = groupHeadings[i + 1];
    const endLine = next ? next.line - 1 : totalLines;
    const sectionLines = lines.slice(h.line - 1, endLine);
    const sectionBytes = Buffer.byteLength(sectionLines.join('\n'), 'utf8');
    sections.push({
      heading: h.text,
      level: h.level,
      startLine: h.line,
      endLine,
      bytes: sectionBytes,
      tokens: Math.max(0, Math.ceil(sectionBytes / CHARS_PER_TOKEN)),
    });
  }
  return { path: filePath, totalLines, bytes, tokens, sections, groupingLevel };
}

interface HeadingInfo {
  line: number; // 1-indexed
  level: number;
  text: string; // includes the leading hashes for display
}

function findHeadings(lines: string[]): HeadingInfo[] {
  const out: HeadingInfo[] = [];
  let inFence = false;
  let fenceMarker = '';
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    const trimmed = line.trim();
    // Skip headings inside fenced code blocks. Match opening/closing fences
    // (``` or ~~~, possibly followed by a language tag).
    const fenceMatch = trimmed.match(/^(```+|~~~+)/);
    if (fenceMatch) {
      const marker = fenceMatch[1]!;
      if (!inFence) {
        inFence = true;
        fenceMarker = marker;
      } else if (trimmed.startsWith(fenceMarker.charAt(0).repeat(fenceMarker.length))) {
        inFence = false;
        fenceMarker = '';
      }
      continue;
    }
    if (inFence) continue;
    const m = line.match(/^(#{1,6})\s+(.*\S)\s*$/);
    if (m) {
      const hashes = m[1]!;
      out.push({ line: i + 1, level: hashes.length, text: `${hashes} ${m[2]}` });
    }
  }
  return out;
}

export function attributeClaudeMd(
  input: AttributeClaudeMdInput,
): ClaudeMdAttributionResult {
  const totalTokens = input.files.reduce((sum, f) => sum + f.tokens, 0);
  if (totalTokens === 0) {
    return {
      totalTokens: 0,
      totalCost: 0,
      sessionCosts: [],
      sectionCosts: [],
      perSessionAvg: 0,
      perSessionP95: 0,
      sessionCount: 0,
    };
  }

  const bySession = new Map<string, TurnRecord[]>();
  for (const t of input.turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }

  const sessionCosts: SessionClaudeMdCost[] = [];
  let totalCost = 0;
  for (const [sessionId, turns] of bySession) {
    turns.sort((a, b) => a.turnIndex - b.turnIndex);
    let cost = 0;
    let ridingTurns = 0;
    const modelCounts = new Map<string, number>();
    for (const t of turns) {
      const rate = lookupRate(t.model, input.pricing);
      if (!rate) continue;
      // Treat CLAUDE.md as residing in cache once any turn reads enough cached
      // tokens to fit it. This is conservative: if a turn's cacheRead is below
      // claude_md_tokens, the file may have been compacted away or this is the
      // first turn (CLAUDE.md is in cacheCreate that turn, not cacheRead).
      if (t.usage.cacheRead < totalTokens) continue;
      cost += (totalTokens / PER_MILLION) * rate.cacheRead;
      ridingTurns++;
      modelCounts.set(t.model, (modelCounts.get(t.model) ?? 0) + 1);
    }
    if (cost > 0) {
      const dominantModel = pickDominantModel(modelCounts);
      sessionCosts.push({
        sessionId,
        cost,
        ridingTurns,
        totalTurns: turns.length,
        model: dominantModel,
      });
      totalCost += cost;
    }
  }

  const sessionCostValues = sessionCosts.map((s) => s.cost).sort((a, b) => a - b);
  const perSessionAvg = sessionCostValues.length === 0
    ? 0
    : sessionCostValues.reduce((a, b) => a + b, 0) / sessionCostValues.length;
  const perSessionP95 = percentile(sessionCostValues, 0.95);

  const sectionCosts: SectionCost[] = [];
  for (const f of input.files) {
    for (const section of f.sections) {
      const tokenShare = section.tokens / totalTokens;
      const totalSecCost = totalCost * tokenShare;
      const perSessionSecCost = perSessionAvg * tokenShare;
      sectionCosts.push({
        filePath: f.path,
        section,
        tokenShare,
        costPerSession: perSessionSecCost,
        totalCost: totalSecCost,
      });
    }
  }
  sectionCosts.sort((a, b) => b.totalCost - a.totalCost);

  return {
    totalTokens,
    totalCost,
    sessionCosts,
    sectionCosts,
    perSessionAvg,
    perSessionP95,
    sessionCount: sessionCosts.length,
  };
}

function pickDominantModel(counts: Map<string, number>): string {
  let bestModel = '';
  let bestCount = -1;
  for (const [m, c] of counts) {
    if (c > bestCount) {
      bestModel = m;
      bestCount = c;
    }
  }
  return bestModel;
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  if (sorted.length === 1) return sorted[0]!;
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[idx]!;
}

function lookupRate(model: string, pricing: PricingTable): ModelCost | undefined {
  const direct = pricing[model];
  if (direct) return direct;
  const i = model.indexOf('/');
  if (i >= 0) {
    const stripped = pricing[model.slice(i + 1)];
    if (stripped) return stripped;
  }
  return undefined;
}

function estimateTokens(text: string): number {
  return Math.max(0, Math.ceil(Buffer.byteLength(text, 'utf8') / CHARS_PER_TOKEN));
}

export interface AdviseRecommendation {
  filePath: string;
  section: MarkdownSection;
  projectedSavingsPerSession: number;
  projectedSavingsAcrossWindow: number;
}

export function buildAdviseRecommendations(
  attribution: ClaudeMdAttributionResult,
  topN = 3,
): AdviseRecommendation[] {
  // Pick the most expensive non-preamble sections as TRIM candidates.
  const candidates = attribution.sectionCosts.filter((s) => s.section.level > 0);
  const top = candidates.slice(0, topN);
  return top.map((s) => ({
    filePath: s.filePath,
    section: s.section,
    projectedSavingsPerSession: s.costPerSession,
    projectedSavingsAcrossWindow: s.totalCost,
  }));
}

export function renderUnifiedDiffForRecommendation(
  filePath: string,
  fileText: string,
  rec: AdviseRecommendation,
): string {
  const lines = fileText.split('\n');
  const start = rec.section.startLine;
  const end = rec.section.endLine;
  const removed = lines.slice(start - 1, end);
  // Use the path inside the project for relative display.
  const display = filePath;
  const header = [`--- a/${display}`, `+++ b/${display}`];
  // Hunk header: original = start, count = removed.length; new = start, count = 0
  const hunk = `@@ -${start},${removed.length} +${start},0 @@`;
  const body = removed.map((l) => `-${l}`).join('\n');
  return [
    `# TRIM: ${rec.section.heading}`,
    `# projected savings per session: $${rec.projectedSavingsPerSession.toFixed(4)}`,
    `# projected savings across window: $${rec.projectedSavingsAcrossWindow.toFixed(4)}`,
    ...header,
    hunk,
    body,
  ].join('\n');
}
