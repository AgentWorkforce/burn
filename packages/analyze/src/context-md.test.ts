import { strict as assert } from 'node:assert';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, describe, it } from 'node:test';

import type { SourceKind, TurnRecord } from '@relayburn/reader';

import { loadClaudeMdFile, parseClaudeMd } from './claude-md.js';
import {
  attributeContext,
  findContextFiles,
  type ContextFile,
  type ParsedContextFile,
} from './context-md.js';
import { loadBuiltinPricing } from './pricing.js';

function turn(over: Partial<TurnRecord> & {
  sessionId: string;
  messageId: string;
  turnIndex: number;
  source: SourceKind;
}): TurnRecord {
  return {
    v: 1,
    model: 'claude-sonnet-4-6',
    ts: '2026-04-23T00:00:00.000Z',
    usage: { input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    ...over,
  };
}

describe('findContextFiles', () => {
  let tmp: string;
  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'ctx-find-'));
  });
  after(async () => {
    await rm(tmp, { recursive: true, force: true });
  });

  it('discovers CLAUDE.md, .claude/CLAUDE.md, and AGENTS.md with the right appliesTo', async () => {
    await writeFile(path.join(tmp, 'CLAUDE.md'), '# root');
    await mkdir(path.join(tmp, '.claude'), { recursive: true });
    await writeFile(path.join(tmp, '.claude', 'CLAUDE.md'), '# nested');
    await writeFile(path.join(tmp, 'AGENTS.md'), '# agents');
    const files = await findContextFiles(tmp);
    assert.equal(files.length, 3);
    const root = files.find((f) => path.basename(f.path) === 'CLAUDE.md' && path.basename(path.dirname(f.path)) !== '.claude')!;
    const nested = files.find((f) => path.basename(f.path) === 'CLAUDE.md' && path.basename(path.dirname(f.path)) === '.claude')!;
    const agents = files.find((f) => path.basename(f.path) === 'AGENTS.md')!;
    assert.deepEqual(root.appliesTo, ['claude-code']);
    assert.deepEqual(nested.appliesTo, ['claude-code']);
    assert.deepEqual(agents.appliesTo, ['codex', 'opencode']);
    assert.equal(root.kind, 'claude-md');
    assert.equal(agents.kind, 'agents-md');
  });
});

describe('attributeContext', () => {
  it('routes Claude Code turns to CLAUDE.md and Codex/OpenCode turns to AGENTS.md', async () => {
    const pricing = await loadBuiltinPricing();
    const rate = pricing['claude-sonnet-4-6']!;

    const claudeMd = parseClaudeMd('/p/CLAUDE.md', '## Claude\n' + 'c'.repeat(4000));
    const agentsMd = parseClaudeMd('/p/AGENTS.md', '## Agents\n' + 'a'.repeat(4000));

    const files: ParsedContextFile[] = [
      {
        file: { kind: 'claude-md', path: '/p/CLAUDE.md', appliesTo: ['claude-code'] },
        parsed: claudeMd,
      },
      {
        file: { kind: 'agents-md', path: '/p/AGENTS.md', appliesTo: ['codex', 'opencode'] },
        parsed: agentsMd,
      },
    ];

    const turns: TurnRecord[] = [
      // Claude Code session — should attribute to CLAUDE.md only.
      turn({
        sessionId: 's-cc',
        messageId: 'm',
        turnIndex: 0,
        source: 'claude-code',
        usage: { input: 10, output: 10, reasoning: 0, cacheRead: claudeMd.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      // Codex session — should attribute to AGENTS.md only.
      turn({
        sessionId: 's-cx',
        messageId: 'm',
        turnIndex: 0,
        source: 'codex',
        usage: { input: 10, output: 10, reasoning: 0, cacheRead: agentsMd.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      // OpenCode session — also AGENTS.md.
      turn({
        sessionId: 's-oc',
        messageId: 'm',
        turnIndex: 0,
        source: 'opencode',
        usage: { input: 10, output: 10, reasoning: 0, cacheRead: agentsMd.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];

    const result = attributeContext({ files, turns, pricing });
    assert.equal(result.perFile.length, 2);

    const claudeAttr = result.perFile.find((p) => p.file.kind === 'claude-md')!;
    const agentsAttr = result.perFile.find((p) => p.file.kind === 'agents-md')!;

    // Claude only sees one Claude Code session.
    assert.equal(claudeAttr.attribution.sessionCount, 1);
    assert.equal(claudeAttr.attribution.sessionCosts[0]!.sessionId, 's-cc');
    const expectedClaude = (claudeMd.tokens / 1_000_000) * rate.cacheRead;
    assert.ok(
      Math.abs(claudeAttr.attribution.totalCost - expectedClaude) <= expectedClaude * 0.10,
    );

    // Agents sees both codex and opencode sessions (2 sessions).
    assert.equal(agentsAttr.attribution.sessionCount, 2);
    const expectedAgents = 2 * (agentsMd.tokens / 1_000_000) * rate.cacheRead;
    assert.ok(
      Math.abs(agentsAttr.attribution.totalCost - expectedAgents) <= expectedAgents * 0.10,
    );

    assert.ok(
      Math.abs(result.grandTotal - (claudeAttr.attribution.totalCost + agentsAttr.attribution.totalCost)) < 1e-9,
    );
  });

  it('does not cross-attribute: Codex turn must not pay for CLAUDE.md', async () => {
    const pricing = await loadBuiltinPricing();
    const claudeMd = parseClaudeMd('/p/CLAUDE.md', '## C\n' + 'x'.repeat(4000));
    const agentsMd = parseClaudeMd('/p/AGENTS.md', '## A\n' + 'y'.repeat(4000));
    const files: ParsedContextFile[] = [
      {
        file: { kind: 'claude-md', path: '/p/CLAUDE.md', appliesTo: ['claude-code'] },
        parsed: claudeMd,
      },
      {
        file: { kind: 'agents-md', path: '/p/AGENTS.md', appliesTo: ['codex', 'opencode'] },
        parsed: agentsMd,
      },
    ];
    const turns: TurnRecord[] = [
      turn({
        sessionId: 's-cx',
        messageId: 'm',
        turnIndex: 0,
        source: 'codex',
        usage: { input: 10, output: 10, reasoning: 0, cacheRead: 50_000, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const result = attributeContext({ files, turns, pricing });
    const claudeAttr = result.perFile.find((p) => p.file.kind === 'claude-md')!;
    // No claude-code turns ⇒ claude-md file has zero cost, zero sessions.
    assert.equal(claudeAttr.attribution.totalCost, 0);
    assert.equal(claudeAttr.attribution.sessionCount, 0);
  });
});

describe('loadContextFile integration', () => {
  let tmp: string;
  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'ctx-load-'));
  });
  after(async () => {
    await rm(tmp, { recursive: true, force: true });
  });

  it('parses AGENTS.md via findContextFiles + loadClaudeMdFile', async () => {
    await writeFile(path.join(tmp, 'AGENTS.md'), '## Section\nbody');
    const files = await findContextFiles(tmp);
    assert.equal(files.length, 1);
    const f = files[0] as ContextFile;
    const parsed = await loadClaudeMdFile(f.path);
    assert.equal(parsed.sections[0]!.heading, '## Section');
  });
});
