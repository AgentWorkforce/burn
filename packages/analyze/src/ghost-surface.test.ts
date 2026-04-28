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
    // code-reviewer + git-commit are observed; forgotten-helper + openspec-apply + openspec-archive are ghosts.
    assert.deepEqual(ghostBasenames, ['forgotten-helper.md', 'openspec-apply.md', 'openspec-archive.md']);
    const helper = claudeGhosts.find((g) => g.path.endsWith('forgotten-helper.md'))!;
    assert.equal(helper.kind, 'ghost-agent');
    assert.equal(helper.sessionCount, 10);
    assert.ok(helper.cost > 0, 'cost is sizeTokens × 10 × rate');
    assert.ok(helper.sizeTokens > 0, 'size is non-zero');
  });

  // #172: a Claude command invoked via `<command-name>/foo</command-name>`
  // de-ghosts the basename even though it never surfaces as a tool call.
  it('de-ghosts a Claude command when its slash form appears in user-turn text', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer', 'git-commit'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 10]]),
      userTurnTextBySession: new Map([
        [
          'claude-code',
          new Map([
            ['session-1', ['<command-name>/openspec-apply</command-name>\nApply the latest proposal.']],
          ]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeGhosts = ghosts.filter((g) => g.source === 'claude-code');
    const ghostBasenames = claudeGhosts.map((g) => path.basename(g.path)).sort();
    // openspec-apply is observed via slash; openspec-archive remains a ghost.
    assert.deepEqual(ghostBasenames, ['forgotten-helper.md', 'openspec-archive.md']);
  });

  // #172: bare `<command-name>foo</command-name>` (no leading slash) is also
  // recognised — Claude has shipped both shapes.
  it('recognises the bare <command-name>foo</command-name> shape (no leading slash)', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer', 'git-commit'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 1]]),
      userTurnTextBySession: new Map([
        [
          'claude-code',
          new Map([['session-1', ['<command-name>openspec-apply</command-name>\nbody']]]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const apply = ghosts.find(
      (g) => g.source === 'claude-code' && g.path.endsWith('openspec-apply.md'),
    );
    assert.equal(apply, undefined, 'claude openspec-apply is de-ghosted');
  });

  // #172: when `userTurnTextBySession` is undefined or empty, slash-command
  // invocations are NOT mined and the detector falls back to v1 behaviour.
  it('falls back to v1 behaviour when userTurnTextBySession is empty', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer', 'git-commit'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 10]]),
      userTurnTextBySession: new Map(),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeGhosts = ghosts.filter((g) => g.source === 'claude-code');
    const ghostBasenames = claudeGhosts.map((g) => path.basename(g.path)).sort();
    // Slash-command mining doesn't run; openspec-apply remains a ghost.
    assert.deepEqual(ghostBasenames, ['forgotten-helper.md', 'openspec-apply.md', 'openspec-archive.md']);
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
    assert.deepEqual(
      [...(byKind.get('ghost-prompt') ?? [])].sort(),
      ['openspec-apply.md', 'openspec-archive.md', 'refactor.md'],
    );
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

  // #172: a Codex prompt invoked via `/<basename>` in a user message
  // de-ghosts the basename even though it never surfaces as a tool call.
  // Canonical issue example: `~/.codex/prompts/openspec-apply.md` typed as
  // `/openspec-apply` in a single session.
  it('de-ghosts a Codex prompt when /<basename> appears in user-turn text', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set(['refactor', 'code-search'])]]),
      sessionCountBySource: new Map([['codex', 5]]),
      userTurnTextBySession: new Map([
        [
          'codex',
          new Map([['session-1', ['/openspec-apply\nApply the latest proposal please.']]]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const codexGhosts = ghosts.filter((g) => g.source === 'codex');
    const apply = codexGhosts.find((g) => g.path.endsWith('openspec-apply.md'));
    assert.equal(apply, undefined, 'codex openspec-apply is de-ghosted via slash mining');
    // openspec-archive (no slash invocation) remains a ghost.
    const archive = codexGhosts.find((g) => g.path.endsWith('openspec-archive.md'));
    assert.ok(archive, 'codex openspec-archive remains a ghost');
  });

  // #172: a slash form not at the start of the message is still recognised
  // (the issue body explicitly calls this out). Codex prepends the prompt
  // body verbatim, but quoted/embedded `/<basename>` references should also
  // count.
  it('recognises a slash invocation that is not at the start of the message', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set()]]),
      sessionCountBySource: new Map([['codex', 1]]),
      userTurnTextBySession: new Map([
        [
          'codex',
          new Map([['session-1', ['Please run the /openspec-apply prompt now.']]]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const apply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.equal(apply, undefined, 'mid-line /openspec-apply still de-ghosts the codex prompt');
  });

  // #172: a hyphenated basename must match the full hyphenated stem; a
  // longer slash command that happens to share a prefix should NOT match.
  // E.g. when only `openspec-apply.md` is installed, `/openspec-apply-foo`
  // should not de-ghost it.
  it('does not match a slash command whose name extends past the stem', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set()]]),
      sessionCountBySource: new Map([['codex', 1]]),
      userTurnTextBySession: new Map([
        ['codex', new Map([['session-1', ['/openspec-apply-foo bar']]])],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const apply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.ok(apply, 'a longer slash command does not de-ghost the shorter stem');
  });

  // #172: slash matches must respect a word boundary on the left, so a path
  // like `https://example.com/openspec-apply` does not de-ghost.
  it('ignores slash forms preceded by a word character (paths/URLs)', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set()]]),
      sessionCountBySource: new Map([['codex', 1]]),
      userTurnTextBySession: new Map([
        [
          'codex',
          new Map([['session-1', ['See https://example.com/openspec-apply for docs.']]]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const apply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.ok(apply, 'URL-style /openspec-apply does not de-ghost the codex prompt');
  });

  // #172: matches are case-insensitive — Codex prompt stems are typically
  // lower-case but the user's typed input may shift case.
  it('matches Codex slash invocations case-insensitively', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['codex', new Set()]]),
      sessionCountBySource: new Map([['codex', 1]]),
      userTurnTextBySession: new Map([
        ['codex', new Map([['session-1', ['/OPENSPEC-Apply now']]])],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const apply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.equal(apply, undefined, 'mixed-case /OPENSPEC-Apply de-ghosts the codex prompt');
  });

  // Regression for Devin's cross-source contamination finding on #172. The
  // Codex slash miner does a literal `/<stem>` search with word-boundary
  // anchors. Claude's `<command-name>` XML wrapper has angle brackets on
  // both sides — neither `<` nor `>` is a word character, so without
  // per-source scoping a Claude `<command-name>/openspec-apply</command-name>`
  // marker would falsely de-ghost an identically-named Codex prompt
  // (`~/.codex/prompts/openspec-apply.md`) that was never invoked under
  // Codex. Only the matching adapter's source should be passed to its
  // `observedNames` hook.
  it('does not de-ghost a Codex prompt from Claude <command-name> markers', async () => {
    const inputs = makeInputs({
      // No Codex tool-call observations, no Codex slash invocations either.
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer'])],
        ['codex', new Set(['refactor'])],
      ]),
      sessionCountBySource: new Map([
        ['claude-code', 1],
        ['codex', 1],
      ]),
      // Claude session has the `<command-name>/openspec-apply</command-name>`
      // marker; Codex sessions are silent. With the contamination bug this
      // marker would leak into the Codex miner and de-ghost the Codex
      // prompt of the same name.
      userTurnTextBySession: new Map([
        [
          'claude-code',
          new Map([
            ['claude-session-1', ['<command-name>/openspec-apply</command-name>\nbody']],
          ]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const codexApply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.ok(
      codexApply,
      'Codex openspec-apply.md must remain a ghost — Claude <command-name> markers must not leak across sources',
    );
    // Sanity: the Claude side is correctly de-ghosted by its own observation.
    const claudeApply = ghosts.find(
      (g) => g.source === 'claude-code' && g.path.endsWith('openspec-apply.md'),
    );
    assert.equal(
      claudeApply,
      undefined,
      'Claude openspec-apply.md is de-ghosted by its own user-turn marker',
    );
  });

  // The mirror case: a Codex `/openspec-apply` invocation must not
  // de-ghost a Claude command of the same name.
  it('does not de-ghost a Claude command from Codex /<stem> matches', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer'])],
        ['codex', new Set(['refactor'])],
      ]),
      sessionCountBySource: new Map([
        ['claude-code', 1],
        ['codex', 1],
      ]),
      userTurnTextBySession: new Map([
        [
          'codex',
          new Map([
            ['codex-session-1', ['/openspec-apply\nApply the latest proposal.']],
          ]),
        ],
      ]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const claudeApply = ghosts.find(
      (g) => g.source === 'claude-code' && g.path.endsWith('openspec-apply.md'),
    );
    assert.ok(
      claudeApply,
      'Claude openspec-apply.md must remain a ghost — Codex slash matches must not leak across sources',
    );
    const codexApply = ghosts.find(
      (g) => g.source === 'codex' && g.path.endsWith('openspec-apply.md'),
    );
    assert.equal(
      codexApply,
      undefined,
      'Codex openspec-apply.md is de-ghosted by its own slash invocation',
    );
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
        ['claude-code', new Set(['Code-Reviewer', 'GIT-COMMIT', 'forgotten-HELPER', 'openspec-archive', 'openspec-apply'])],
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

  it('uses per-session cost for severity and usdPerSession, not cumulative', async () => {
    // High session count × small per-session cost — cumulative would cross
    // the `high` severity threshold, but the per-session impact is `info`.
    const inputs = makeInputs({
      observedNamesBySource: new Map([
        ['claude-code', new Set(['code-reviewer'])],
      ]),
      sessionCountBySource: new Map([['claude-code', 100_000]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const helper = ghosts.find((g) => g.path.endsWith('forgotten-helper.md'))!;
    // Sanity check: cumulative cost is well above the `high` threshold ($0.50).
    assert.ok(helper.cost > 1, `expected cumulative cost > $1, got ${helper.cost}`);
    // Per-session cost is sizeTokens × rate (≈ small number × 1e-6 ≈ <$0.05).
    assert.ok(
      helper.costPerSession < 0.05,
      `per-session cost should be below warn threshold, got ${helper.costPerSession}`,
    );
    const finding = ghostSurfaceToFinding(helper, { archiveDir: '/tmp/ghost-archive' });
    assert.equal(finding.estimatedSavings.usdPerSession, helper.costPerSession);
    assert.equal(finding.severity, 'info');
  });

  it('shell-quotes paths with spaces in the mv command', async () => {
    const ghost = {
      source: 'claude-code' as const,
      kind: 'ghost-agent' as const,
      path: '/Users/me/.claude/agents/my helper.md',
      sizeTokens: 100,
      cost: 0.001,
      costPerSession: 0.0001,
      sessionCount: 10,
    };
    const finding = ghostSurfaceToFinding(ghost, { archiveDir: '/tmp/ghost archive' });
    const action = finding.actions[0]!;
    assert.equal(action.type, 'command');
    assert.ok(action.text.includes(`'/Users/me/.claude/agents/my helper.md'`));
    assert.ok(action.text.includes(`'/tmp/ghost archive'`));
  });

  it('emits a paste action (not mv) for synthetic OpenCode JSON-pointer paths', async () => {
    const inputs = makeInputs({
      observedNamesBySource: new Map([['opencode', new Set(['deploy'])]]),
      sessionCountBySource: new Map([['opencode', 5]]),
    });
    const ghosts = await detectGhostSurface(inputs);
    const synthetic = ghosts.find((g) => g.path.includes('#/commands/ghost-command'))!;
    const finding = ghostSurfaceToFinding(synthetic);
    const action = finding.actions[0]!;
    assert.equal(action.type, 'paste');
    assert.ok(!action.text.includes('mv '), 'paste action should not invoke mv');
    assert.ok(action.text.includes('opencode.json'));
    assert.ok(action.text.includes('/commands/ghost-command'));
  });
});
