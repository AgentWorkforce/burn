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

export type OverheadFileKind = 'claude-md' | 'agents-md';

export interface OverheadFile {
  kind: OverheadFileKind;
  path: string;
  // Which agent sources read this file into their cached context. A turn's
  // `source` must be in this list for the file to count toward that turn.
  appliesTo: SourceKind[];
}

export interface ParsedOverheadFile {
  file: OverheadFile;
  parsed: ParsedClaudeMd;
}

export interface OverheadFileAttribution {
  file: OverheadFile;
  parsed: ParsedClaudeMd;
  attribution: ClaudeMdAttributionResult;
}

export interface OverheadAttribution {
  perFile: OverheadFileAttribution[];
  grandTotal: number;
  // Count of distinct turns that contributed to at least one file's cost. Not
  // the sum of per-file ridingTurns (a turn could ride along in multiple
  // files, e.g. CLAUDE.md + .claude/CLAUDE.md).
  totalRidingTurns: number;
}

export interface AttributeOverheadInput {
  files: ParsedOverheadFile[];
  turns: TurnRecord[];
  pricing: PricingTable;
}

// Files we discover for any project. The `appliesTo` encodes which agent
// reads the file into its cached prompt prefix — a Codex session doesn't pay
// for CLAUDE.md, and a Claude Code session doesn't pay for AGENTS.md, so
// attribution must filter turns by source.
interface Candidate {
  kind: OverheadFileKind;
  relativePath: string;
  appliesTo: SourceKind[];
}

const CANDIDATES: readonly Candidate[] = [
  { kind: 'claude-md', relativePath: 'CLAUDE.md', appliesTo: ['claude-code'] },
  { kind: 'claude-md', relativePath: '.claude/CLAUDE.md', appliesTo: ['claude-code'] },
  { kind: 'agents-md', relativePath: 'AGENTS.md', appliesTo: ['codex', 'opencode'] },
];

export async function findOverheadFiles(projectPath: string): Promise<OverheadFile[]> {
  const out: OverheadFile[] = [];
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

export async function loadOverheadFile(file: OverheadFile): Promise<ParsedOverheadFile> {
  const parsed = await loadClaudeMdFile(file.path);
  return { file, parsed };
}

export function attributeOverhead(input: AttributeOverheadInput): OverheadAttribution {
  const perFile: OverheadFileAttribution[] = [];
  // Per-session max ridingTurns across every file. The eviction check is
  // `cacheRead >= file_tokens`, so a smaller file always rides along for a
  // superset of the turns that a larger file rides along for (same source,
  // same session). Taking the max per session gives the correct count of
  // distinct turns without double-counting when CLAUDE.md + .claude/CLAUDE.md
  // both attribute to the same Claude Code session.
  const maxRidingBySession = new Map<string, number>();

  for (const { file, parsed } of input.files) {
    const filteredTurns = input.turns.filter((t) => file.appliesTo.includes(t.source));
    const attribution = attributeClaudeMd({
      files: [parsed],
      turns: filteredTurns,
      pricing: input.pricing,
    });
    perFile.push({ file, parsed, attribution });

    for (const sc of attribution.sessionCosts) {
      const prev = maxRidingBySession.get(sc.sessionId) ?? 0;
      if (sc.ridingTurns > prev) maxRidingBySession.set(sc.sessionId, sc.ridingTurns);
    }
  }

  const grandTotal = perFile.reduce((sum, f) => sum + f.attribution.totalCost, 0);

  let totalRidingTurns = 0;
  for (const n of maxRidingBySession.values()) totalRidingTurns += n;

  return { perFile, grandTotal, totalRidingTurns };
}

export function describeAppliesTo(appliesTo: SourceKind[]): string {
  return appliesTo.slice().sort().join(', ');
}
