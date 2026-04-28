import { strict as assert } from 'node:assert';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, it } from 'node:test';

import type { SourceKind } from '@relayburn/reader';

import {
  claudeGhostAdapter,
  codexGhostAdapter,
  detectGhostSurface,
  ghostSurfaceToFinding,
  opencodeGhostAdapter,
  type GhostSurfaceInputs,
} from './ghost-surface.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// Tests run against built dist/, so the fixtures path must walk up out of
// dist/ to the repo root.
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'ghost-surface');

const CLAUDE_HOME = path.join(FIXTURES, 'claude');
const CODEX_HOME = path.join(FIXTURES, 'codex');
const OPENCODE_PROJECT = path.join(FIXTURES, 'opencode-project');

// A flat dollar-per-token rate. Picked so the math is easy to verify in the
// assertions: 1e-6 USD/token = $1 per million tokens. The actual rate the
// CLI plugs in comes from the pricing table.
const RATE = 1e-6;

function makeInputs(overrides: Partial<GhostSurfaceInputs> = {}): GhostSurfaceInputs {
  const observed = new Map<SourceKind, Set<string>>();
  if (overrides.observedNamesBySource) {
    for (const [k, v] of overrides.observedNamesBySource) observed.set(k, v);
  }
  const sessionCount = new Map<SourceKind, number>();
  if (overrides.sessionCountBySource) {
    for (const [k, v] of overrides.sessionCountBySource) sessionCount.set(k, v);
  }
  return {
    observedNamesBySource: observed,
    sessionCountBySource: sessionCount,
    dollarPerToken: overrides.dollarPerToken ?? RATE,
    claudeHome: overrides.claudeHome ?? CLAUDE_HOME,
    codexHome: overrides.codexHome ?? CODEX_HOME,
    opencodeProjects: overrides.opencodeProjects ?? [OPENCODE_PROJECT],
    ...(overrides.userTurnTextBySession !== undefined
      ? { userTurnTextBySession: overrides.userTurnTextBySession }
      : {}),
  };
}

describe('claudeGhostAdapter', () => {
  it('enumerates agents/skills/commands as candidates', async () => {
    const candidates = await claudeGhostAdapter.enumerate(makeInputs());
    const kinds = new Set(candidates.map((c) => c.kind));
    assert.ok(kinds.has('ghost-agent'), 'has agents');
    assert.ok(kinds.has('ghost-skill'), 'has skills');
    assert.ok(kinds.has('ghost-command'), 'has commands');
    const agents = candidates.filter((c) => c.kind === 'ghost-agent').map((c) => c.basename).sort();
    assert.deepEqual(agents, ['code-reviewer.md', 'forgotten-helper.md']);
  });

  it('returns an empty list when claudeHome does not exist', async () => {
    const candidates = await claudeGhostAdapter.enumerate(
      makeInputs({ claudeHome: path.join(FIXTURES, 'does-not-exist') }),
    );
    assert.equal(candidates.length, 0);
  });

  it('detects a ghost agent when its basename is not in observedNames', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer', 'git-commit'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 10]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeGhosts = ghosts.filter((g) => g.source === 'claude-code');
    const ghostBasenames = claudeGhosts.map((g) => path.basename(g.path)).sort();
    // code-reviewer + git-commit are observed; forgotten-helper + openspec-archive are ghosts.
    assert.deepEqual(ghostBasenames, ['forgotten-helper.md', 'openspec-archive.md']);
    const helper = claudeGhosts.find((g) => g.path.endsWith('forgotten-helper.md'))!;
    assert.equal(helper.kind, 'ghost-agent');
    assert.equal(helper.sessionCount, 10);
    assert.ok(helper.cost > 0, 'cost is sizeTokens × 10 × rate');
    assert.ok(helper.sizeTokens > 0, 'size is non-zero');
  });
});

describe('codexGhostAdapter', () => {
  it('enumerates prompts/skills/rules/memories', async () => {
    const candidates = await codexGhostAdapter.enumerate(makeInputs());
    const byKind = new Map<string, string[]>();
    for (const c of candidates) {
      const list = byKind.get(c.kind) ?? [];
      list.push(c.basename);
      byKind.set(c.kind, list);
    }
    assert.deepEqual([...(byKind.get('ghost-prompt') ?? [])].sort(), ['openspec-archive.md', 'refactor.md']);
    assert.deepEqual(byKind.get('ghost-skill'), ['code-search.md']);
    assert.deepEqual(byKind.get('ghost-rule'), ['no-print.md']);
    assert.deepEqual(byKind.get('ghost-memory'), ['preferences.md']);
  });

  it('flags Codex prompts/openspec-archive.md as a ghost (canonical issue example)', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set(['refactor', 'code-search'])]]),
      sessionCountBySource: new Map([['codex', 5]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const codexGhosts = ghosts.filter((g) => g.source === 'codex');
    const openspec = codexGhosts.find((g) => g.path.endsWith('openspec-archive.md'));
    assert.ok(openspec, 'codex openspec-archive.md is reported as a ghost');
    assert.equal(openspec!.kind, 'ghost-prompt');
    assert.equal(openspec!.sessionCount, 5);
    assert.ok(openspec!.cost > 0);
    // Rules / memories also surface — they ride in every system prompt and
    // weren't observed as tools either.
    const kinds = new Set(codexGhosts.map((g) => g.kind));
    assert.ok(kinds.has('ghost-rule'), 'codex rules surface as ghosts');
    assert.ok(kinds.has('ghost-memory'), 'codex memories surface as ghosts');
  });
});

describe('opencodeGhostAdapter', () => {
  it('enumerates declared skills, declared commands, and project-skills folder', async () => {
    const candidates = await opencodeGhostAdapter.enumerate(makeInputs());
    const declared = candidates.filter((c) => c.countedByCatalogBloat === true);
    const project = candidates.filter((c) => c.countedByCatalogBloat !== true);
    const declaredNames = declared.map((c) => c.basename).sort();
    assert.deepEqual(
      declaredNames,
      ['abandoned-helper', 'code-search'],
      'declared catalog skills are flagged with countedByCatalogBloat',
    );
    const projectSkills = project.filter((c) => c.kind === 'ghost-skill').map((c) => c.basename);
    assert.deepEqual(projectSkills, ['project-skill.md']);
    const commands = project.filter((c) => c.kind === 'ghost-command').map((c) => c.basename).sort();
    assert.deepEqual(commands, ['deploy', 'ghost-command']);
  });

  it('emits cost: 0 for declared catalog-bloat skills (#54 dedup)', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['opencode', new Set(['code-search', 'deploy'])]]),
      sessionCountBySource: new Map([['opencode', 20]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const opencodeGhosts = ghosts.filter((g) => g.source === 'opencode');
    const abandoned = opencodeGhosts.find((g) => g.path.includes('abandoned-helper'));
    assert.ok(abandoned, 'declared catalog skill abandoned-helper is reported');
    assert.equal(abandoned!.cost, 0, 'cost is zeroed to avoid double-count with #54');
    assert.equal(abandoned!.countedByCatalogBloat, true);
    const ghostCmd = opencodeGhosts.find((g) => g.path.endsWith('#/commands/ghost-command'));
    assert.ok(ghostCmd, 'custom command ghost-command is reported');
    assert.ok(ghostCmd!.cost > 0, 'custom command cost is computed normally');
    assert.equal(ghostCmd!.countedByCatalogBloat, undefined);
    const projectSkill = opencodeGhosts.find((g) => g.path.endsWith('project-skill.md'));
    assert.ok(projectSkill, 'project skill is reported');
    assert.ok(projectSkill!.cost > 0, 'project skill cost is computed normally');
    assert.equal(projectSkill!.countedByCatalogBloat, undefined);
  });
});

describe('detectGhostSurface — orchestrator', () => {
  it('runs every adapter and returns a single sorted list', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer', 'git-commit'])],
        ['codex', new Set(['refactor', 'code-search'])],
        ['opencode', new Set(['code-search', 'deploy'])],
      ]),
      sessionCountBySource: new Map([
        ['claude-code', 10],
        ['codex', 5],
        ['opencode', 20],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    // Sort invariant: cost descending.
    for (let i = 1; i < ghosts.length; i++) {
      assert.ok(ghosts[i - 1]!.cost >= ghosts[i]!.cost, 'sorted by cost desc');
    }
    const sources = new Set(ghosts.map((g) => g.source));
    assert.ok(sources.has('claude-code'));
    assert.ok(sources.has('codex'));
    assert.ok(sources.has('opencode'));
  });

  it('treats observed names case-insensitively', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['Code-Reviewer', 'GIT-COMMIT', 'forgotten-HELPER', 'openspec-archive'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 1]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeGhosts = ghosts.filter((g) => g.source === 'claude-code');
    assert.equal(claudeGhosts.length, 0, 'all Claude entries match case-insensitively');
  });

  it('includes a ghost when sessionCount is 0 (still surfaced, cost is 0)', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['claude-code', new Set()]]),
      sessionCountBySource: new Map(),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeGhosts = ghosts.filter((g) => g.source === 'claude-code');
    assert.ok(claudeGhosts.length > 0);
    for (const g of claudeGhosts) {
      assert.equal(g.cost, 0);
      assert.equal(g.sessionCount, 0);
    }
  });
});

describe('ghostSurfaceToFinding', () => {
  it('produces a WasteFinding with a `mv` command action', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 10]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const helper = ghosts.find((g) => g.path.endsWith('forgotten-helper.md'))!;
    const finding = ghostSurfaceToFinding(helper, { archiveDir: '/tmp/ghost-archive' });
    assert.equal(finding.kind, 'ghost-agent');
    assert.equal(finding.actions.length, 1);
    const action = finding.actions[0]!;
    assert.equal(action.type, 'command');
    assert.ok(action.text.includes('mv '));
    assert.ok(action.text.includes('/tmp/ghost-archive'));
    assert.ok(action.text.includes(helper.path));
    assert.ok(finding.title.includes('forgotten-helper'));
    assert.ok(finding.detail.includes('claude-code'));
  });

  it('marks catalog-bloat findings with cost: 0 and notes the dedup in detail', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['opencode', new Set(['deploy'])]]),
      sessionCountBySource: new Map([['opencode', 100]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const abandoned = ghosts.find((g) => g.path.includes('abandoned-helper'))!;
    const finding = ghostSurfaceToFinding(abandoned);
    assert.equal(finding.estimatedSavings.usdPerSession, 0);
    assert.ok(finding.detail.includes('catalog-bloat'));
  });
});
