import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { classifyActivity, countRetries, normalizeToolName, parseBashCommand } from './classifier.js';
import type { ToolCall } from './types.js';

const tc = (name: string, target?: string, id = name): ToolCall => {
  const call: ToolCall = { id, name, argsHash: 'h' };
  if (target !== undefined) call.target = target;
  return call;
};

describe('classifyActivity — tool-pattern classification', () => {
  it('classifies turns that spawn a subagent as delegation', () => {
    // Delegation dominates even when an Edit is also present: spawning the
    // subagent is the headline activity and describes the work this turn did.
    const r = classifyActivity({
      toolCalls: [tc('Agent', 'Explore'), tc('Edit', '/a.ts')],
    });
    assert.equal(r.activity, 'delegation');
  });

  it('classifies ExitPlanMode turns as planning', () => {
    const r = classifyActivity({ toolCalls: [tc('ExitPlanMode')] });
    assert.equal(r.activity, 'planning');
  });

  it('classifies bash test runners as testing', () => {
    const cases = ['pytest', 'vitest run', 'bun test', 'npm test', 'go test ./...', 'cargo test', 'node --test', 'make test'];
    for (const cmd of cases) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'testing', `expected 'testing' for ${cmd}, got ${r.activity}`);
    }
  });

  it('classifies read-only git / PR inspection commands as review', () => {
    for (const cmd of ['git status', 'git diff --stat', 'git show HEAD~1', 'gh pr diff 123', 'gh pr view 123']) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'review', cmd);
    }
  });

  it('ignores leading env assignments when matching bash patterns', () => {
    const r = classifyActivity({ toolCalls: [tc('Bash', 'CI=1 NODE_ENV=test pytest -q')] });
    assert.equal(r.activity, 'testing');
  });

  it('classifies git commands as git', () => {
    for (const cmd of ['git push origin main', 'git commit -m "x"', 'git rebase main']) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'git', cmd);
    }
  });

  it('classifies build and deploy commands as build-deploy', () => {
    for (const cmd of ['npm run build', 'docker build .', 'cargo build --release', 'kubectl apply -f k8s/', 'terraform apply', 'make build']) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'build-deploy', cmd);
    }
  });

  it('classifies lint / typecheck commands as verification', () => {
    const cases = ['npm run lint', 'eslint .', 'ruff check src/', 'cargo check', 'tsc --noEmit', 'make lint', 'prettier --check .', 'cargo fmt --check'];
    for (const cmd of cases) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'verification', `expected verification for: ${cmd}`);
    }
  });

  it('avoids build-deploy false positives for non-deploy shell commands', () => {
    assert.equal(classifyActivity({ toolCalls: [tc('Bash', 'make lint')] }).activity, 'verification');
    assert.equal(classifyActivity({ toolCalls: [tc('Bash', 'kubectl logs deploy/api')] }).activity, 'exploration');
  });

  it('classifies plain edit turns as coding', () => {
    const r = classifyActivity({ toolCalls: [tc('Edit', '/a.ts'), tc('Write', '/b.ts')] });
    assert.equal(r.activity, 'coding');
    assert.equal(r.hasEdits, true);
  });

  it('classifies read-only tool turns as exploration', () => {
    const r = classifyActivity({ toolCalls: [tc('Read', '/a.ts'), tc('Grep', 'foo')] });
    assert.equal(r.activity, 'exploration');
  });
});

describe('parseBashCommand', () => {
  const normalized = (cmd: string): string => {
    const parsed = parseBashCommand(cmd);
    assert.ok(parsed, `expected parse for ${cmd}`);
    return parsed.normalized;
  };

  it('normalizes representative classifier Bash pattern examples', () => {
    const cases: Array<[string, string]> = [
      ['pytest', 'pytest'],
      ['python -m pytest -q', 'pytest'],
      ['vitest run', 'vitest'],
      ['bun test', 'bun test'],
      ['npm test', 'npm test'],
      ['pnpm run test', 'pnpm test'],
      ['go test ./...', 'go test'],
      ['cargo test', 'cargo test'],
      ['node --test', 'node --test'],
      ['make test', 'make test'],
      ['git status', 'git status'],
      ['git -C repo diff --stat', 'git diff'],
      ['git show HEAD~1', 'git show'],
      ['gh pr diff 123', 'gh pr diff'],
      ['gh pr view 123', 'gh pr view'],
      ['gh run view 99', 'gh run view'],
      ['git push origin main', 'git push'],
      ['git commit -m "x"', 'git commit'],
      ['npm install', 'npm install'],
      ['pip3 uninstall foo', 'pip3 uninstall'],
      ['python -m pip install black', 'pip install'],
      ['uv pip install ruff', 'uv pip install'],
      ['go mod tidy', 'go mod tidy'],
      ['brew update', 'brew update'],
      ['apt-get install jq', 'apt-get install'],
      ['prettier --check .', 'prettier'],
      ['eslint . --fix', 'eslint'],
      ['cargo fmt --check', 'cargo fmt'],
      ['tsc --noEmit', 'tsc'],
      ['./node_modules/.bin/eslint .', 'eslint'],
      ['docker compose build api', 'docker compose build'],
      ['docker build .', 'docker build'],
      ['kubectl apply -f k8s/', 'kubectl apply'],
      ['terraform plan', 'terraform plan'],
      ['make build', 'make build'],
    ];

    for (const [cmd, expected] of cases) {
      assert.equal(normalized(cmd), expected, cmd);
    }
  });

  it('handles env, cd, subshell, shell-c, and first-segment wrappers', () => {
    const cases: Array<[string, string]> = [
      ['CI=1 NODE_ENV=test pytest -q', 'pytest'],
      ['cd /tmp && git status', 'git status'],
      ['cd /tmp; git status', 'git status'],
      ['(git status)', 'git status'],
      ['bash -c "git status"', 'git status'],
      ['bash -lc "git status"', 'git status'],
      ['bash --norc -c "git status"', 'git status'],
      ['sh -c "pnpm run test"', 'pnpm test'],
      ['git status | cat', 'git status'],
      ['git status && git diff', 'git status'],
      ['git status; git diff', 'git status'],
    ];

    for (const [cmd, expected] of cases) {
      assert.equal(normalized(cmd), expected, cmd);
    }
  });

  it('returns shell buckets for compound shell forms and null for empty input', () => {
    assert.equal(parseBashCommand('   '), null);
    assert.deepEqual(parseBashCommand('for f in *; do echo "$f"; done'), {
      binary: '(shell)',
      normalized: '(shell)',
    });
    assert.deepEqual(parseBashCommand('if true; then git status; fi'), {
      binary: '(shell)',
      normalized: '(shell)',
    });
    assert.deepEqual(parseBashCommand('cat <<EOF\nhello\nEOF'), {
      binary: '(shell)',
      normalized: '(shell)',
    });
  });
});

describe('classifyActivity — keyword refinement', () => {
  it('refines edits into debugging when prompt mentions errors', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts')],
      text: 'fix the bug that crashes on null input',
    });
    assert.equal(r.activity, 'debugging');
  });

  it('refines edits into debugging when a tool call failed this turn', () => {
    // A subsequent tool_result with is_error flips an otherwise plain "coding"
    // turn into debugging — the model is reacting to a real failure.
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts')],
      text: 'make this work',
      hasFailedTool: true,
    });
    assert.equal(r.activity, 'debugging');
  });

  it('refines edits into refactoring when prompt mentions refactor terms', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts')],
      text: 'please refactor this to extract the helper',
    });
    assert.equal(r.activity, 'refactoring');
  });

  it('refines edits into feature when prompt mentions add/implement', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts'), tc('Write', '/b.ts')],
      text: 'add a new endpoint for /users',
    });
    assert.equal(r.activity, 'feature');
  });

  it('prioritizes debugging over feature keywords when both appear', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts')],
      text: 'add handling to fix the bug where requests crash',
    });
    assert.equal(r.activity, 'debugging');
  });

  it('promotes exploration into debugging when a tool call errored', () => {
    const r = classifyActivity({
      toolCalls: [tc('Read', '/a.ts')],
      hasFailedTool: true,
    });
    assert.equal(r.activity, 'debugging');
  });

  it('promotes failed bash test/git/build to debugging', () => {
    // A failing pytest / git push / npm run build is the model reacting to an
    // error, not neutrally running the command — debugging should win over the
    // bash-pattern category.
    const cases: Array<[string, string]> = [
      ['pytest', 'testing'],
      ['git push origin main', 'git'],
      ['npm run build', 'build-deploy'],
    ];
    for (const [cmd] of cases) {
      const ok = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      const failed = classifyActivity({ toolCalls: [tc('Bash', cmd)], hasFailedTool: true });
      assert.notEqual(ok.activity, 'debugging', `${cmd} without failure should not be debugging`);
      assert.equal(failed.activity, 'debugging', `${cmd} with failure should be debugging`);
    }
  });

  it('promotes exploration to feature when the prompt asks to add something', () => {
    // Adversarial case from the issue: a turn that looks like exploration
    // (only Read/Grep) but the ask is clearly a feature request — keyword
    // refinement should move it off exploration.
    const r = classifyActivity({
      toolCalls: [tc('Read', '/a.ts'), tc('Grep', 'router')],
      text: 'add a new route for /admin',
    });
    assert.equal(r.activity, 'feature');
  });

  it('promotes read-only turns to review when the prompt explicitly asks for review', () => {
    const r = classifyActivity({
      toolCalls: [tc('Read', '/a.ts'), tc('Grep', 'auth')],
      text: 'review this authentication change for risks',
    });
    assert.equal(r.activity, 'review');
  });
});

describe('classifyActivity — no-tool fallback', () => {
  it('returns brainstorming for idea-exploration prompts with no tools', () => {
    const r = classifyActivity({ toolCalls: [], text: 'what if we tried a different approach here?' });
    assert.equal(r.activity, 'brainstorming');
  });

  it('returns planning for planning prompts with no tools', () => {
    const r = classifyActivity({ toolCalls: [], text: "let's outline the roadmap for Q3" });
    assert.equal(r.activity, 'planning');
  });

  it('returns debugging when text mentions bugs even without tools', () => {
    const r = classifyActivity({ toolCalls: [], text: 'the build is broken with a null pointer error' });
    assert.equal(r.activity, 'debugging');
  });

  it('falls back to conversation when nothing matches', () => {
    const r = classifyActivity({ toolCalls: [], text: 'thanks, that looks good' });
    assert.equal(r.activity, 'conversation');
  });

  it('falls back to conversation on empty input', () => {
    const r = classifyActivity({ toolCalls: [] });
    assert.equal(r.activity, 'conversation');
    assert.equal(r.retries, 0);
    assert.equal(r.hasEdits, false);
  });
});

describe('countRetries', () => {
  it('counts a single edit→bash→edit cycle as one retry', () => {
    assert.equal(
      countRetries([tc('Edit', '/a.ts', '1'), tc('Bash', 'npm test', '2'), tc('Edit', '/a.ts', '3')]),
      1,
    );
  });

  it('counts two retries in edit→bash→edit→bash→edit', () => {
    assert.equal(
      countRetries([
        tc('Edit', '/a.ts', '1'),
        tc('Bash', 'npm test', '2'),
        tc('Edit', '/a.ts', '3'),
        tc('Bash', 'npm test', '4'),
        tc('Edit', '/a.ts', '5'),
      ]),
      2,
    );
  });

  it('counts retries across different files (edit→bash→edit of a different file)', () => {
    // Adversarial case: the retry signal is about *rework*, not same-file.
    // Touching b.ts after testing a.ts still indicates a reactive edit.
    assert.equal(
      countRetries([tc('Edit', '/a.ts', '1'), tc('Bash', 'npm test', '2'), tc('Edit', '/b.ts', '3')]),
      1,
    );
  });

  it('returns 0 for two consecutive edits with no bash between them', () => {
    assert.equal(countRetries([tc('Edit', '/a.ts', '1'), tc('Edit', '/b.ts', '2')]), 0);
  });

  it('returns 0 when bash appears before any edit', () => {
    assert.equal(countRetries([tc('Bash', 'ls', '1'), tc('Edit', '/a.ts', '2')]), 0);
  });

  it('is surfaced on ClassificationResult for edit-heavy turns', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/a.ts', '1'), tc('Bash', 'npm test', '2'), tc('Edit', '/a.ts', '3')],
    });
    assert.equal(r.retries, 1);
    assert.equal(r.hasEdits, true);
    assert.equal(r.activity, 'coding');
  });
});

describe('classifyActivity — hasEdits flag', () => {
  it('is true whenever any edit tool is present', () => {
    assert.equal(classifyActivity({ toolCalls: [tc('Edit', '/a.ts')] }).hasEdits, true);
    assert.equal(classifyActivity({ toolCalls: [tc('Write', '/a.ts')] }).hasEdits, true);
    assert.equal(classifyActivity({ toolCalls: [tc('NotebookEdit', '/a.ipynb')] }).hasEdits, true);
  });

  it('is false when only read-only or bash tools are present', () => {
    assert.equal(classifyActivity({ toolCalls: [tc('Read', '/a.ts')] }).hasEdits, false);
    assert.equal(classifyActivity({ toolCalls: [tc('Bash', 'ls')] }).hasEdits, false);
  });
});

describe('classifyActivity — new categories', () => {
  it('classifies an edit turn touching only doc files as docs', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/project/README.md'), tc('Write', '/project/docs/guide.md')],
    });
    assert.equal(r.activity, 'docs');
    assert.equal(r.hasEdits, true);
  });

  it('stays coding when a code file is mixed in with docs', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/project/README.md'), tc('Edit', '/project/src/a.ts')],
    });
    assert.equal(r.activity, 'coding');
  });

  it('keeps doc-only edits as docs even when the prompt uses feature wording', () => {
    const r = classifyActivity({
      toolCalls: [tc('Edit', '/project/README.md')],
      text: 'add an installation section to the README',
    });
    assert.equal(r.activity, 'docs');
  });

  it('classifies npm install / pip install / cargo add as deps', () => {
    const cases = [
      'npm install react',
      'pnpm add lodash',
      'yarn add -D typescript',
      'pip install requests',
      'uv add httpx',
      'poetry add fastapi',
      'cargo add serde',
      'go get github.com/foo/bar',
      'bundle install',
      'brew install jq',
    ];
    for (const cmd of cases) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'deps', `expected deps for: ${cmd}`);
    }
  });

  it('classifies formatter invocations as format', () => {
    const cases = [
      'prettier --write .',
      'eslint . --fix',
      'biome check src/ --apply',
      'ruff format src/',
      'black .',
      'cargo fmt',
      'gofmt -w .',
      'rustfmt src/lib.rs',
    ];
    for (const cmd of cases) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'format', `expected format for: ${cmd}`);
    }
  });

  it('keeps "npm test" as testing even though npm also triggers deps pattern', () => {
    // Ordering matters: TEST_PATTERNS must be checked before DEPS_PATTERNS so
    // `npm test` doesn't collide with `npm install`.
    const r = classifyActivity({ toolCalls: [tc('Bash', 'npm test')] });
    assert.equal(r.activity, 'testing');
  });

  it('classifies playwright / cypress / puppeteer bash commands as testing', () => {
    for (const cmd of ['playwright test', 'cypress run', 'puppeteer']) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'testing', `expected testing for: ${cmd}`);
    }
  });

  it('classifies high-retry edit turns as debugging even without error signal', () => {
    // Edit → bash → edit → bash → edit → bash → edit = 3 retries inside one turn.
    // Model is clearly chasing a bug, so call it debugging without needing an
    // explicit "fix the bug" keyword.
    const r = classifyActivity({
      toolCalls: [
        tc('Edit', '/a.ts', '1'),
        tc('Bash', 'pytest', '2'),
        tc('Edit', '/a.ts', '3'),
        tc('Bash', 'pytest', '4'),
        tc('Edit', '/a.ts', '5'),
        tc('Bash', 'pytest', '6'),
        tc('Edit', '/a.ts', '7'),
      ],
    });
    assert.equal(r.activity, 'debugging');
    assert.equal(r.hasEdits, true);
    assert.ok(r.retries >= 2);
  });

  it('falls back to reasoning for tool-less turns with reasoning tokens', () => {
    const r = classifyActivity({ toolCalls: [], reasoningTokens: 5000 });
    assert.equal(r.activity, 'reasoning');
  });

  it('still returns conversation for tool-less turns with no reasoning and no keywords', () => {
    const r = classifyActivity({ toolCalls: [], reasoningTokens: 0, text: 'thanks' });
    assert.equal(r.activity, 'conversation');
  });

  it('keyword signal still wins over reasoning fallback', () => {
    const r = classifyActivity({
      toolCalls: [],
      reasoningTokens: 10000,
      text: 'fix the crash in the login handler',
    });
    assert.equal(r.activity, 'debugging');
  });
});

describe('classifyActivity — cross-harness tool aliasing', () => {
  it('treats codex apply_patch as an edit', () => {
    const r = classifyActivity({ toolCalls: [tc('apply_patch', '/a.ts')] });
    assert.equal(r.hasEdits, true);
    assert.equal(r.activity, 'coding');
  });

  it('treats codex subagent management tools as delegation', () => {
    assert.equal(classifyActivity({ toolCalls: [tc('spawn_agent')] }).activity, 'delegation');
    assert.equal(classifyActivity({ toolCalls: [tc('wait_agent')] }).activity, 'delegation');
  });

  it('treats codex exec_command as bash for test/git/build detection', () => {
    assert.equal(
      classifyActivity({ toolCalls: [tc('exec_command', 'pytest -q')] }).activity,
      'testing',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('exec_command', 'git push origin main')] }).activity,
      'git',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('shell', 'npm run build')] }).activity,
      'build-deploy',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('exec_command', 'git diff --stat')] }).activity,
      'review',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('shell', 'cargo check')] }).activity,
      'verification',
    );
  });

  it('treats opencode lowercase tool names the same as claude names', () => {
    assert.equal(
      classifyActivity({ toolCalls: [tc('edit', '/a.ts')] }).hasEdits,
      true,
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('bash', 'vitest')] }).activity,
      'testing',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('read', '/a.ts'), tc('grep', 'foo')] }).activity,
      'exploration',
    );
    assert.equal(
      classifyActivity({ toolCalls: [tc('task', 'explore')] }).activity,
      'delegation',
    );
  });

  it('counts retries across harness-normalized edit/bash cycles', () => {
    // Codex style: apply_patch → exec_command → apply_patch → exec_command → apply_patch
    assert.equal(
      countRetries([
        tc('apply_patch', '/a.ts', '1'),
        tc('exec_command', 'pytest', '2'),
        tc('apply_patch', '/a.ts', '3'),
        tc('exec_command', 'pytest', '4'),
        tc('apply_patch', '/a.ts', '5'),
      ]),
      2,
    );
  });

  it('normalizeToolName returns the name unchanged when no alias exists', () => {
    assert.equal(normalizeToolName('Edit'), 'Edit');
    assert.equal(normalizeToolName('mcp_thing'), 'mcp_thing');
  });
});
