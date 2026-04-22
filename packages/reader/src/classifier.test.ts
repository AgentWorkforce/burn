import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { classifyActivity, countRetries } from './classifier.js';
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
    const cases = ['pytest', 'vitest run', 'bun test', 'npm test', 'go test ./...', 'cargo test', 'node --test'];
    for (const cmd of cases) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'testing', `expected 'testing' for ${cmd}, got ${r.activity}`);
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
    for (const cmd of ['npm run build', 'docker build .', 'cargo build --release', 'kubectl apply -f k8s/', 'terraform apply']) {
      const r = classifyActivity({ toolCalls: [tc('Bash', cmd)] });
      assert.equal(r.activity, 'build-deploy', cmd);
    }
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
