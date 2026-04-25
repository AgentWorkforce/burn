import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { buildMcpConfig } from './config.js';

interface ParsedMcpConfig {
  mcpServers: {
    burn: {
      type: string;
      command: string;
      args: string[];
    };
  };
}

function parse(raw: string): ParsedMcpConfig {
  return JSON.parse(raw) as ParsedMcpConfig;
}

describe('buildMcpConfig', () => {
  const SESSION_ID = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';

  it('registers a stdio "burn" MCP server pointing at `burn mcp-server`', () => {
    const { config } = buildMcpConfig({ sessionId: SESSION_ID });
    const parsed = parse(config);
    assert.equal(parsed.mcpServers.burn.type, 'stdio');
    assert.equal(parsed.mcpServers.burn.command, 'burn');
    assert.deepEqual(parsed.mcpServers.burn.args, [
      'mcp-server',
      '--session-id',
      SESSION_ID,
    ]);
  });

  it('honors a custom burnBin path', () => {
    const { config } = buildMcpConfig({
      sessionId: SESSION_ID,
      burnBin: '/opt/tools/burn',
    });
    const parsed = parse(config);
    assert.equal(parsed.mcpServers.burn.command, '/opt/tools/burn');
  });

  it('produces a JSON string that round-trips through JSON.parse', () => {
    const { config } = buildMcpConfig({ sessionId: SESSION_ID });
    assert.doesNotThrow(() => JSON.parse(config));
  });

  it('bakes the session id into args so tools default to the running session', () => {
    const { config } = buildMcpConfig({ sessionId: SESSION_ID });
    const parsed = parse(config);
    const idIdx = parsed.mcpServers.burn.args.indexOf('--session-id');
    assert.notEqual(idIdx, -1);
    assert.equal(parsed.mcpServers.burn.args[idIdx + 1], SESSION_ID);
  });
});
