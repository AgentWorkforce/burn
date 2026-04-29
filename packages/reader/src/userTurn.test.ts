import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createUserTurnTokenCounter } from './userTurn.js';

describe('createUserTurnTokenCounter', () => {
  it('defaults to cl100k', async () => {
    const counter = await createUserTurnTokenCounter();
    assert.equal(counter.tokenizer, 'cl100k');
  });

  it('rejects unsupported runtime tokenizer values with a clear error', async () => {
    await assert.rejects(
      createUserTurnTokenCounter('gpt2' as never),
      /Unsupported user-turn tokenizer: gpt2/,
    );
  });
});
