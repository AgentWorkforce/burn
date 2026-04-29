import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { listHarnessNames, lookupHarness } from './registry.js';

describe('harness registry', () => {
  it('lists the known harnesses', () => {
    const names = listHarnessNames();
    assert.deepEqual([...names].sort(), ['claude', 'codex', 'opencode']);
  });

  it('looks up adapters by name', async () => {
    const claude = await lookupHarness('claude');
    assert.ok(claude);
    assert.equal(claude!.name, 'claude');

    const codex = await lookupHarness('codex');
    assert.ok(codex);
    assert.equal(codex!.name, 'codex');

    const opencode = await lookupHarness('opencode');
    assert.ok(opencode);
    assert.equal(opencode!.name, 'opencode');
  });

  it('returns undefined for unknown harnesses', async () => {
    assert.equal(await lookupHarness('cursor'), undefined);
    assert.equal(await lookupHarness(''), undefined);
    assert.equal(await lookupHarness('claude '), undefined);
  });
});
