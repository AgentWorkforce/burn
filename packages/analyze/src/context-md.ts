import { stat } from 'node:fs/promises';
import * as path from 'node:path';

import type { SourceKind, TurnRecord } from '@relayburn/reader';

import {
  attributeClaudeMd,
  loadClaudeMdFile,
  type ClaudeMdAttributionResult,
  type ParsedClaudeMd,
} from './claude-md.js';
import type { PricingTable } from './pricing.js';

export type ContextFileKind = 'claude-md' | 'agents-md';

export interface ContextFile {
  kind: ContextFileKind;
  path: string;
  // Which agent sources read this file into their cached context. A turn's
  // `source` must be in this list for the file to count toward that turn.
  appliesTo: SourceKind[];
}

export interface ParsedContextFile {
  file: ContextFile;
  parsed: ParsedClaudeMd;
}

export interface ContextFileAttribution {
  file: ContextFile;
  parsed: ParsedClaudeMd;
  attribution: ClaudeMdAttributionResult;
}

export interface ContextAttributionResult {
  perFile: ContextFileAttribution[];
  grandTotal: number;
  // Count of distinct turns that contributed to at least one file's cost. Not
  // the sum of per-file ridingTurns (a turn could ride along in multiple
  // files, e.g. CLAUDE.md + .claude/CLAUDE.md).
  totalRidingTurns: number;
}

export interface AttributeContextInput {
  files: ParsedContextFile[];
  turns: TurnRecord[];
  pricing: PricingTable;
}

// Files we discover for any project. The `appliesTo` encodes which agent
// reads the file into its cached prompt prefix — a Codex session doesn't pay
// for CLAUDE.md, and a Claude Code session doesn't pay for AGENTS.md, so
// attribution must filter turns by source.
interface Candidate {
  kind: ContextFileKind;
  relativePath: string;
  appliesTo: SourceKind[];
}

const CANDIDATES: readonly Candidate[] = [
  { kind: 'claude-md', relativePath: 'CLAUDE.md', appliesTo: ['claude-code'] },
  { kind: 'claude-md', relativePath: '.claude/CLAUDE.md', appliesTo: ['claude-code'] },
  { kind: 'agents-md', relativePath: 'AGENTS.md', appliesTo: ['codex', 'opencode'] },
];

export async function findContextFiles(projectPath: string): Promise<ContextFile[]> {
  const out: ContextFile[] = [];
  for (const c of CANDIDATES) {
    const abs = path.join(projectPath, c.relativePath);
    try {
      const st = await stat(abs);
      if (st.isFile()) {
        out.push({ kind: c.kind, path: abs, appliesTo: [...c.appliesTo] });
      }
    } catch {
      // not present; skip silently
    }
  }
  return out;
}

export async function loadContextFile(file: ContextFile): Promise<ParsedContextFile> {
  const parsed = await loadClaudeMdFile(file.path);
  return { file, parsed };
}

export function attributeContext(input: AttributeContextInput): ContextAttributionResult {
  const perFile: ContextFileAttribution[] = [];
  const ridingTurnKeys = new Set<string>();

  for (const { file, parsed } of input.files) {
    const filteredTurns = input.turns.filter((t) => file.appliesTo.includes(t.source));
    const attribution = attributeClaudeMd({
      files: [parsed],
      turns: filteredTurns,
      pricing: input.pricing,
    });
    perFile.push({ file, parsed, attribution });

    for (const sc of attribution.sessionCosts) {
      if (sc.ridingTurns > 0) ridingTurnKeys.add(`${sc.sessionId}:${sc.ridingTurns}`);
    }
  }

  const grandTotal = perFile.reduce((sum, f) => sum + f.attribution.totalCost, 0);

  // Rough estimate — avoids double-counting the same session across files that
  // share a source (e.g. CLAUDE.md + .claude/CLAUDE.md both apply to Claude
  // Code), by keying on session-ridingTurn-count which is stable per session.
  let totalRidingTurns = 0;
  for (const key of ridingTurnKeys) {
    const parts = key.split(':');
    const n = Number(parts[parts.length - 1]);
    if (Number.isFinite(n)) totalRidingTurns += n;
  }

  return { perFile, grandTotal, totalRidingTurns };
}

export function describeAppliesTo(appliesTo: SourceKind[]): string {
  return appliesTo.slice().sort().join(', ');
}
