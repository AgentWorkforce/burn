import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import {
  SPAWN_ENV_TAG_KEYS,
  mergeSpawnTags,
  readSpawnEnvTags,
  spawnTagEnvOverrides,
} from './spawn-tags.js';

describe('spawn-tags env contract', () => {
  it('reads every documented RELAYBURN_* env var into its canonical tag key', () => {
    const env = {
      RELAYBURN_WORKFLOW_ID: 'wf-refactor-auth',
      RELAYBURN_STEP_ID: 'step-3',
      RELAYBURN_AGENT_ID: 'ag-42',
      RELAYBURN_PARENT_AGENT_ID: 'ag-root',
      RELAYBURN_PERSONA: 'senior-eng',
      RELAYBURN_TIER: 'best',
    };
    assert.deepEqual(readSpawnEnvTags(env), {
      workflowId: 'wf-refactor-auth',
      stepId: 'step-3',
      agentId: 'ag-42',
      parentAgentId: 'ag-root',
      persona: 'senior-eng',
      tier: 'best',
    });
  });

  it('drops empty-string env values so an unset parent var does not pollute the stamp', () => {
    const env = {
      RELAYBURN_WORKFLOW_ID: 'wf-1',
      RELAYBURN_AGENT_ID: '',
    };
    assert.deepEqual(readSpawnEnvTags(env), { workflowId: 'wf-1' });
  });

  it('ignores unrelated RELAYBURN_* internals like HOME/SESSION_ID/CONTENT_STORE', () => {
    const env = {
      RELAYBURN_HOME: '/tmp/burn',
      RELAYBURN_SESSION_ID: 'abc',
      RELAYBURN_CONTENT_STORE: 'hash-only',
      RELAYBURN_AGENT_ID: 'ag-42',
    };
    assert.deepEqual(readSpawnEnvTags(env), { agentId: 'ag-42' });
  });

  it('returns empty when no spawn-tag env vars are set', () => {
    assert.deepEqual(readSpawnEnvTags({}), {});
  });

  it('merges with CLI tags taking precedence over env tags on key collision', () => {
    const envTags = { workflowId: 'wf-from-env', agentId: 'ag-from-env' };
    const cliTags = { agentId: 'ag-from-flag', persona: 'senior-eng' };
    assert.deepEqual(mergeSpawnTags(envTags, cliTags), {
      workflowId: 'wf-from-env',
      agentId: 'ag-from-flag', // CLI flag wins
      persona: 'senior-eng',
    });
  });

  it('builds env overrides for the child harness from the merged tag bag', () => {
    const final = {
      workflowId: 'wf-1',
      agentId: 'ag-42',
      // unrelated stamps must not leak into the env block
      harness: 'codex',
      burnSpawn: '1',
      burnSpawnTs: '2026-04-22T00:00:00.000Z',
    };
    assert.deepEqual(spawnTagEnvOverrides(final), {
      RELAYBURN_WORKFLOW_ID: 'wf-1',
      RELAYBURN_AGENT_ID: 'ag-42',
    });
  });

  it('round-trips: env -> read -> merge -> overrides preserves transitive tags', () => {
    const parentEnv = {
      RELAYBURN_WORKFLOW_ID: 'wf-1',
      RELAYBURN_AGENT_ID: 'ag-parent',
    };
    const cliTags = { agentId: 'ag-child', persona: 'senior-eng' };
    const merged = mergeSpawnTags(readSpawnEnvTags(parentEnv), cliTags);
    const childEnv = spawnTagEnvOverrides(merged);
    assert.equal(childEnv['RELAYBURN_WORKFLOW_ID'], 'wf-1');
    assert.equal(childEnv['RELAYBURN_AGENT_ID'], 'ag-child');
    assert.equal(childEnv['RELAYBURN_PERSONA'], 'senior-eng');
  });

  it('exposes the canonical key list so callers and docs cannot drift', () => {
    const tags = SPAWN_ENV_TAG_KEYS.map((p) => p.tag);
    assert.deepEqual(tags, [
      'workflowId',
      'stepId',
      'agentId',
      'parentAgentId',
      'persona',
      'tier',
    ]);
  });
});
