import { strict as assert } from 'node:assert';
import { mkdtemp, writeFile, mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import {
  attributeClaudeMd,
  buildTrimRecommendations,
  findClaudeMdFiles,
  loadClaudeMdFile,
  parseClaudeMd,
  renderUnifiedDiffForRecommendation,
} from './claude-md.js';
import { loadBuiltinPricing } from './pricing.js';

function turn(over: Partial<TurnRecord> & { sessionId: string; messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-23T00:00:00.000Z',
    usage: { input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    ...over,
  };
}

describe('parseClaudeMd', () => {
  it('returns a single preamble section for a file with no headings', () => {
    const text = 'just a paragraph\nwith some content';
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    assert.equal(parsed.sections.length, 1);
    assert.equal(parsed.sections[0]!.level, 0);
    assert.equal(parsed.sections[0]!.heading, '(preamble)');
    assert.equal(parsed.groupingLevel, 0);
  });

  it('groups by H2 when H2 sections exist, treating leading content as preamble', () => {
    const text = [
      '# Title',
      'intro paragraph',
      '',
      '## Architecture',
      'arch line 1',
      'arch line 2',
      '',
      '## Testing',
      'testing line 1',
    ].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    assert.equal(parsed.groupingLevel, 2);
    // preamble + 2 H2 sections
    assert.equal(parsed.sections.length, 3);
    assert.equal(parsed.sections[0]!.level, 0);
    assert.equal(parsed.sections[1]!.heading, '## Architecture');
    assert.equal(parsed.sections[2]!.heading, '## Testing');
    // line ranges
    assert.equal(parsed.sections[1]!.startLine, 4);
    assert.equal(parsed.sections[1]!.endLine, 7);
    assert.equal(parsed.sections[2]!.startLine, 8);
    assert.equal(parsed.sections[2]!.endLine, 9);
  });

  it('groups by H1 when no H2 exists', () => {
    const text = ['# Section A', 'a body', '# Section B', 'b body'].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    assert.equal(parsed.groupingLevel, 1);
    assert.equal(parsed.sections.length, 2);
    assert.equal(parsed.sections[0]!.heading, '# Section A');
    assert.equal(parsed.sections[1]!.heading, '# Section B');
  });

  it('ignores headings inside fenced code blocks', () => {
    const text = [
      '## Real heading',
      'body',
      '',
      '```',
      '## not a heading',
      '```',
      '',
      '## Another real heading',
    ].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    assert.equal(parsed.sections.length, 2);
    assert.equal(parsed.sections[0]!.heading, '## Real heading');
    assert.equal(parsed.sections[1]!.heading, '## Another real heading');
  });

  it('a "```python" line inside a 3-backtick fence does not close the fence', () => {
    // A line that starts with backticks but has trailing non-whitespace must
    // NOT be treated as a closing fence (per CommonMark). Otherwise nested
    // code samples inside a CLAUDE.md fence corrupt the section boundaries.
    const text = [
      '```',
      '## inside block',
      '````python',
      '## should-be-inside',
      '```',
      '## should-be-outside',
    ].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    const headings = parsed.sections
      .filter((s) => s.level > 0)
      .map((s) => s.heading);
    assert.deepEqual(headings, ['## should-be-outside']);
  });

  it('does not count a trailing newline as an extra line', () => {
    const parsed = parseClaudeMd('/p/CLAUDE.md', '## Section\nbody\n');
    assert.equal(parsed.totalLines, 2);
    assert.equal(parsed.sections[0]!.endLine, 2);
  });

  it('normalizes CRLF line endings', () => {
    const parsed = parseClaudeMd('/p/CLAUDE.md', '## A\r\nbody\r\n## B\r\nb\r\n');
    assert.equal(parsed.sections.length, 2);
    assert.equal(parsed.sections[0]!.heading, '## A');
    assert.equal(parsed.sections[1]!.heading, '## B');
  });

  it('returns zero sections for empty input', () => {
    const parsed = parseClaudeMd('/p/CLAUDE.md', '');
    assert.equal(parsed.totalLines, 0);
    assert.equal(parsed.sections.length, 0);
  });
});

describe('attributeClaudeMd', () => {
  it('attributes per-turn cost within ±10% of hand-computed truth', async () => {
    const pricing = await loadBuiltinPricing();
    const rate = pricing['claude-sonnet-4-6']!;

    // Construct a CLAUDE.md sized to a known token count.
    // 4000 chars / 4 = ~1000 tokens.
    const TOKENS = 1000;
    const text = '# Title\n' + 'x'.repeat(4000 - 8); // roughly 1000 tokens
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);

    // 5 turns, each with cacheRead well above CLAUDE.md size.
    const sessionId = 's-cm-1';
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 5; i++) {
      turns.push(turn({
        sessionId,
        messageId: `m-${i}`,
        turnIndex: i,
        usage: {
          input: 50,
          output: 30,
          reasoning: 0,
          cacheRead: parsed.tokens + 5000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }));
    }

    const result = attributeClaudeMd({ files: [parsed], turns, pricing });
    // Expected: 5 turns × (parsed.tokens / 1M) × cacheRead price
    const expected = 5 * (parsed.tokens / 1_000_000) * rate.cacheRead;
    assert.ok(
      Math.abs(result.totalCost - expected) <= expected * 0.10,
      `total=${result.totalCost} expected=${expected} diff>10%`,
    );
    assert.equal(result.sessionCount, 1);
    assert.equal(result.sessionCosts[0]!.ridingTurns, 5);
  });

  it('section cost is proportional to its token share', async () => {
    const pricing = await loadBuiltinPricing();
    const text = [
      '## Big',
      'x'.repeat(8000),
      '## Small',
      'x'.repeat(2000),
    ].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    const sessionId = 's-cm-sec';
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(turn({
        sessionId,
        messageId: `m-${i}`,
        turnIndex: i,
        usage: { input: 50, output: 10, reasoning: 0, cacheRead: parsed.tokens + 1000, cacheCreate5m: 0, cacheCreate1h: 0 },
      }));
    }
    const result = attributeClaudeMd({ files: [parsed], turns, pricing });
    const big = result.sectionCosts.find((s) => s.section.heading === '## Big')!;
    const small = result.sectionCosts.find((s) => s.section.heading === '## Small')!;
    assert.ok(big);
    assert.ok(small);
    assert.ok(big.totalCost > small.totalCost);
    // Ratios should match token ratios within rounding (preamble adds little).
    const ratio = big.totalCost / small.totalCost;
    const tokenRatio = big.section.tokens / small.section.tokens;
    assert.ok(Math.abs(ratio - tokenRatio) / tokenRatio < 0.05);
  });

  it('skips turns where cacheRead is below CLAUDE.md size (not in cache)', async () => {
    const pricing = await loadBuiltinPricing();
    const text = '## Big\n' + 'x'.repeat(40_000); // ~10k tokens
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    const sessionId = 's-cm-skip';
    const turns: TurnRecord[] = [
      // turn 0: cacheRead too small — skipped
      turn({
        sessionId,
        messageId: 'm0',
        turnIndex: 0,
        usage: { input: 5000, output: 10, reasoning: 0, cacheRead: 100, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      // turn 1: cacheRead enough — counted
      turn({
        sessionId,
        messageId: 'm1',
        turnIndex: 1,
        usage: { input: 50, output: 10, reasoning: 0, cacheRead: parsed.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const result = attributeClaudeMd({ files: [parsed], turns, pricing });
    assert.equal(result.sessionCosts[0]!.ridingTurns, 1);
  });

  it('returns zero-cost when CLAUDE.md is empty', () => {
    const parsed = parseClaudeMd('/p/CLAUDE.md', '');
    const result = attributeClaudeMd({
      files: [parsed],
      turns: [turn({ sessionId: 's', messageId: 'm', turnIndex: 0 })],
      pricing: {},
    });
    assert.equal(result.totalCost, 0);
    assert.equal(result.sessionCosts.length, 0);
  });

  it('includes zero-cost sessions in sessionCount so avg/p95 are not biased upward', async () => {
    const pricing = await loadBuiltinPricing();
    const text = '## Body\n' + 'x'.repeat(4000);
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    const turns: TurnRecord[] = [
      // Session A: CLAUDE.md in cache
      turn({ sessionId: 's-A', messageId: 'm', turnIndex: 0, usage: { input: 10, output: 10, reasoning: 0, cacheRead: parsed.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 } }),
      // Session B: no cacheRead — zero attributed cost but should still count.
      turn({ sessionId: 's-B', messageId: 'm', turnIndex: 0, usage: { input: 500, output: 10, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 } }),
    ];
    const result = attributeClaudeMd({ files: [parsed], turns, pricing });
    assert.equal(result.sessionCount, 2);
    const b = result.sessionCosts.find((s) => s.sessionId === 's-B')!;
    assert.equal(b.cost, 0);
    assert.equal(b.ridingTurns, 0);
    // Average should be half of session A's cost.
    const a = result.sessionCosts.find((s) => s.sessionId === 's-A')!;
    assert.ok(Math.abs(result.perSessionAvg - a.cost / 2) < 1e-9);
  });

  it('sum of section costs stays ≤ totalCost (byte-share is additive)', async () => {
    const pricing = await loadBuiltinPricing();
    // Many small sections would over-allocate if we used ceil(bytes/4)/tokens.
    const parts: string[] = [];
    for (let i = 0; i < 20; i++) parts.push(`## Section ${i}\n${'x'.repeat(123)}\n`);
    const parsed = parseClaudeMd('/p/CLAUDE.md', parts.join(''));
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 5; i++) {
      turns.push(turn({ sessionId: 's-sum', messageId: `m${i}`, turnIndex: i, usage: { input: 10, output: 10, reasoning: 0, cacheRead: parsed.tokens + 500, cacheCreate5m: 0, cacheCreate1h: 0 } }));
    }
    const result = attributeClaudeMd({ files: [parsed], turns, pricing });
    const sumOfSectionCosts = result.sectionCosts.reduce((a, b) => a + b.totalCost, 0);
    assert.ok(sumOfSectionCosts <= result.totalCost + 1e-9);
    // Shares should sum to exactly 1 (bytes are additive).
    const sumShares = result.sectionCosts.reduce((a, b) => a + b.tokenShare, 0);
    assert.ok(Math.abs(sumShares - 1) < 1e-9);
  });
});

describe('findClaudeMdFiles', () => {
  let tmp: string;
  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'cm-find-'));
  });
  after(async () => {
    await rm(tmp, { recursive: true, force: true });
  });

  it('finds root CLAUDE.md and .claude/CLAUDE.md', async () => {
    await writeFile(path.join(tmp, 'CLAUDE.md'), '# Root');
    await mkdir(path.join(tmp, '.claude'), { recursive: true });
    await writeFile(path.join(tmp, '.claude', 'CLAUDE.md'), '# Nested');
    const files = await findClaudeMdFiles(tmp);
    assert.equal(files.length, 2);
    assert.ok(
      files.some(
        (f) => path.basename(f) === 'CLAUDE.md' && path.basename(path.dirname(f)) !== '.claude',
      ),
    );
    assert.ok(
      files.some(
        (f) => path.basename(f) === 'CLAUDE.md' && path.basename(path.dirname(f)) === '.claude',
      ),
    );
  });

  it('loads parsed content via loadClaudeMdFile', async () => {
    const target = path.join(tmp, 'CLAUDE.md');
    await writeFile(target, '## Section\nbody');
    const parsed = await loadClaudeMdFile(target);
    assert.equal(parsed.sections[0]!.heading, '## Section');
  });
});

describe('buildTrimRecommendations + renderUnifiedDiffForRecommendation', () => {
  it('emits a TRIM diff for the largest section that hand-applies cleanly', async () => {
    const pricing = await loadBuiltinPricing();
    const text = [
      '## Big',
      'x'.repeat(8000),
      '## Small',
      'x'.repeat(2000),
    ].join('\n');
    const parsed = parseClaudeMd('/p/CLAUDE.md', text);
    const sessionId = 's-cm-advise';
    const turns: TurnRecord[] = [
      turn({ sessionId, messageId: 'm0', turnIndex: 0, usage: { input: 50, output: 10, reasoning: 0, cacheRead: parsed.tokens + 1000, cacheCreate5m: 0, cacheCreate1h: 0 } }),
    ];
    const attribution = attributeClaudeMd({ files: [parsed], turns, pricing });
    const recs = buildTrimRecommendations(attribution, 1);
    assert.equal(recs.length, 1);
    assert.equal(recs[0]!.section.heading, '## Big');
    const diff = renderUnifiedDiffForRecommendation('/p/CLAUDE.md', text, recs[0]!);
    assert.ok(diff.includes('# TRIM: ## Big'));
    assert.ok(diff.includes('--- a/'));
    assert.ok(diff.includes('+++ b/'));
    assert.ok(diff.includes('@@ -1,2 +1,0 @@'));
  });

  it('emits a project-relative POSIX path in the diff header when baseDir is given', async () => {
    const pricing = await loadBuiltinPricing();
    const text = '## Only\nbody\n';
    const parsed = parseClaudeMd('/home/u/repo/CLAUDE.md', text);
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm', turnIndex: 0, usage: { input: 10, output: 10, reasoning: 0, cacheRead: parsed.tokens + 100, cacheCreate5m: 0, cacheCreate1h: 0 } }),
    ];
    const attribution = attributeClaudeMd({ files: [parsed], turns, pricing });
    const recs = buildTrimRecommendations(attribution, 1);
    const diff = renderUnifiedDiffForRecommendation(
      '/home/u/repo/CLAUDE.md',
      text,
      recs[0]!,
      '/home/u/repo',
    );
    // Should NOT have a leading slash ("--- a//...") and should be relative.
    assert.ok(diff.includes('--- a/CLAUDE.md'));
    assert.ok(diff.includes('+++ b/CLAUDE.md'));
    assert.ok(!diff.includes('a//'));
  });
});
