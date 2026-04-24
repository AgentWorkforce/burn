import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { parseArgs } from './args.js';

describe('parseArgs', () => {
  it('parses flags with values', () => {
    const a = parseArgs(['--since', '7d', '--project', '/foo']);
    assert.equal(a.flags['since'], '7d');
    assert.equal(a.flags['project'], '/foo');
  });

  it('treats flags without following value as boolean', () => {
    const a = parseArgs(['--verbose', '--since', '1h']);
    assert.equal(a.flags['verbose'], true);
    assert.equal(a.flags['since'], '1h');
  });

  it('parses --tag k=v pairs into tags map', () => {
    const a = parseArgs(['--tag', 'workflow=refactor', '--tag', 'user=will']);
    assert.deepEqual(a.tags, { workflow: 'refactor', user: 'will' });
  });

  it('splits passthrough at --', () => {
    const a = parseArgs(['--tag', 'k=v', '--', '--resume', 'abc']);
    assert.deepEqual(a.tags, { k: 'v' });
    assert.deepEqual(a.passthrough, ['--resume', 'abc']);
  });

  it('preserves positional args before --', () => {
    const a = parseArgs(['foo', '--since', '1h', 'bar']);
    assert.deepEqual(a.positional, ['foo', 'bar']);
  });

  it('parses --foo=bar inline value form', () => {
    const a = parseArgs(['--patterns=retries,failures', '--json']);
    assert.equal(a.flags['patterns'], 'retries,failures');
    assert.equal(a.flags['json'], true);
  });

  it('treats --foo= as empty string value', () => {
    const a = parseArgs(['--patterns=']);
    assert.equal(a.flags['patterns'], '');
  });
});
