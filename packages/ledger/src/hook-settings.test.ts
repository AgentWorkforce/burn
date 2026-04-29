import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { buildClaudeHookSettings } from './hook-settings.js';

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

interface HookCommand {
  type: string;
  command: string;
}
interface HookMatcher {
  matcher?: string;
  hooks: HookCommand[];
}
interface ParsedSettings {
  hooks: Record<string, HookMatcher[]>;
}

function parse(raw: string): ParsedSettings {
  return JSON.parse(raw) as ParsedSettings;
}

describe('buildClaudeHookSettings', () => {
  it('returns a fresh UUID session id each call', () => {
    const a = buildClaudeHookSettings();
    const b = buildClaudeHookSettings();
    assert.match(a.sessionId, UUID_RE);
    assert.match(b.sessionId, UUID_RE);
    assert.notEqual(a.sessionId, b.sessionId);
  });

  it('registers every supported Claude Code hook event pointing at burn ingest', () => {
    const { settings } = buildClaudeHookSettings();
    const parsed = parse(settings);
    const expected = [
      'PreToolUse',
      'PostToolUse',
      'UserPromptSubmit',
      'Notification',
      'Stop',
      'SubagentStop',
      'SessionEnd',
    ];
    for (const evt of expected) {
      const matchers = parsed.hooks[evt];
      assert.ok(matchers && matchers.length > 0, `missing hook for ${evt}`);
      const cmd = matchers[0]!.hooks[0]!;
      assert.equal(cmd.type, 'command');
      assert.equal(cmd.command, 'burn ingest --hook claude --quiet');
    }
  });

  it('does not register PostToolUseFailure — tool failures flow through PostToolUse', () => {
    const { settings } = buildClaudeHookSettings();
    const parsed = parse(settings);
    assert.equal(
      parsed.hooks['PostToolUseFailure'],
      undefined,
      'PostToolUseFailure is not a Claude Code hook event; registering it would invalidate --settings',
    );
  });

  it('only applies a tool matcher to tool-scoped events', () => {
    const { settings } = buildClaudeHookSettings();
    const parsed = parse(settings);
    assert.equal(parsed.hooks['PreToolUse']![0]!.matcher, '*');
    assert.equal(parsed.hooks['PostToolUse']![0]!.matcher, '*');
    for (const evt of ['UserPromptSubmit', 'Notification', 'Stop', 'SubagentStop', 'SessionEnd']) {
      assert.equal(
        parsed.hooks[evt]![0]!.matcher,
        undefined,
        `${evt} is not tool-scoped and should not carry a matcher`,
      );
    }
  });

  it('honors a custom burnBin path', () => {
    const { settings } = buildClaudeHookSettings({ burnBin: '/opt/tools/burn' });
    const parsed = parse(settings);
    const cmd = parsed.hooks['PostToolUse']![0]!.hooks[0]!.command;
    assert.equal(cmd, '/opt/tools/burn ingest --hook claude --quiet');
  });

  it('shell-quotes a burnBin path that contains spaces', () => {
    const { settings } = buildClaudeHookSettings({ burnBin: '/opt/path with space/burn' });
    const parsed = parse(settings);
    const cmd = parsed.hooks['PreToolUse']![0]!.hooks[0]!.command;
    assert.equal(cmd, `'/opt/path with space/burn' ingest --hook claude --quiet`);
  });

  it('produces settings that round-trip through JSON.parse', () => {
    const { settings } = buildClaudeHookSettings();
    assert.doesNotThrow(() => JSON.parse(settings));
  });
});
