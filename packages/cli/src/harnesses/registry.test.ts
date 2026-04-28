import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { listHarnessNames, lookupHarness } from './registry.js';

describe('harness registry', () => {
  it('lists the known harnesses', () => {
    const names = listHarnessNames();
    assert.deepEqual([...names].sort(), ['claude', 'codex', 'opencode']);
  });

  it('looks up adapters by name', () => {
    const claude = lookupHarness('claude');
    assert.ok(claude);
    assert.equal(claude!.name, 'claude');

    const codex = lookupHarness('codex');
    assert.ok(codex);
    assert.equal(codex!.name, 'codex');

    const opencode = lookupHarness('opencode');
    assert.ok(opencode);
    assert.equal(opencode!.name, 'opencode');
  });

  it('returns undefined for unknown harnesses', () => {
    assert.equal(lookupHarness('cursor'), undefined);
    assert.equal(lookupHarness(''), undefined);
    assert.equal(lookupHarness('claude '), undefined);
  });
});
