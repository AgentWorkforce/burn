import { strict as assert } from 'node:assert';
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { createOpencodeStreamIngestor } from './opencode-stream.js';
import { parseOpencodeSession } from './opencode.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'opencode');

function sessionFile(fixture: string, sessionId: string): string {
  return path.join(FIXTURES, fixture, 'storage', 'session', 'global', `${sessionId}.json`);
}

describe('opencode stream ingestor', () => {
  it('normalizes a stream-owned OpenCode session into burn records on idle', async () => {
    const ingestor = await createOpencodeStreamIngestor({
      contentMode: 'full',
      tokenizer: 'heuristic',
    });

    await ingestor.ingest(
      {
        type: 'session.created',
        properties: { info: { id: 'ses_stream', directory: '/tmp/project' } },
      },
      '1',
    );
    await ingestor.ingest({
      type: 'message.updated',
      properties: {
        info: {
          id: 'msg_stream_user',
          sessionID: 'ses_stream',
          role: 'user',
          time: { created: 1_777_000_000_000 },
        },
      },
    });
    await ingestor.ingest({
      type: 'message.part.updated',
      properties: {
        part: {
          id: 'prt_user_text',
          sessionID: 'ses_stream',
          messageID: 'msg_stream_user',
          type: 'text',
          text: 'list files',
        },
      },
    });
    await ingestor.ingest({
      type: 'message.updated',
      properties: {
        info: {
          id: 'msg_stream_asst',
          sessionID: 'ses_stream',
          role: 'assistant',
          time: { created: 1_777_000_001_000 },
          providerID: 'anthropic',
          modelID: 'claude-sonnet-4-5',
          path: { cwd: '/tmp/project' },
          tokens: {
            input: 10,
            output: 20,
            reasoning: 0,
            cache: { read: 5, write: 7 },
          },
        },
      },
    });
    await ingestor.ingest({
      type: 'message.part.updated',
      properties: {
        part: {
          id: 'prt_asst_tool',
          sessionID: 'ses_stream',
          messageID: 'msg_stream_asst',
          type: 'tool',
          callID: 'toolu_bash_1',
          tool: 'bash',
          state: {
            status: 'completed',
            input: { command: 'ls' },
            output: 'a.ts\n',
          },
        },
      },
    });
    await ingestor.ingest({
      type: 'message.part.updated',
      properties: {
        part: {
          id: 'prt_step_finish',
          sessionID: 'ses_stream',
          messageID: 'msg_stream_asst',
          type: 'step-finish',
          reason: 'end_turn',
          tokens: {
            input: 10,
            output: 20,
            reasoning: 0,
            cache: { read: 5, write: 7 },
          },
        },
      },
    });

    const result = await ingestor.ingest(
      {
        type: 'session.idle',
        properties: { sessionID: 'ses_stream' },
      },
      '7',
    );

    assert.equal(result.turns.length, 1);
    assert.equal(result.turns[0]!.messageId, 'msg_stream_asst');
    assert.equal(result.turns[0]!.turnIndex, 0);
    assert.equal(result.turns[0]!.toolCalls[0]!.name, 'bash');
    assert.equal(result.turns[0]!.toolCalls[0]!.target, 'ls');
    assert.deepEqual(result.turns[0]!.usage, {
      input: 10,
      output: 20,
      reasoning: 0,
      cacheRead: 5,
      cacheCreate5m: 7,
      cacheCreate1h: 0,
    });

    assert.equal(result.toolResultEvents.length, 1);
    const event = result.toolResultEvents[0]!;
    assert.equal(event.toolUseId, 'toolu_bash_1');
    assert.equal(event.eventIndex, 0);
    assert.equal(event.usageAttribution, 'single-tool-turn');
    assert.equal(event.usage?.input, 10);
    assert.equal(event.contentLength, 'a.ts\n'.length);

    assert.equal(result.content.filter((c) => c.kind === 'tool_result').length, 1);
    assert.equal(result.content.filter((c) => c.kind === 'text').length, 1);
    assert.equal(result.userTurns.length, 1);
    assert.equal(result.userTurns[0]!.blocks[0]!.kind, 'text');
    assert.equal(result.cursor.lastEventId, '7');

    const secondIdle = await ingestor.ingest({
      type: 'session.idle',
      properties: { sessionID: 'ses_stream' },
    });
    assert.equal(secondIdle.turns.length, 0);
    assert.equal(secondIdle.toolResultEvents.length, 0);
  });

  it('does not emit direct records for sessions that predate the stream', async () => {
    const ingestor = await createOpencodeStreamIngestor({ contentMode: 'full' });
    await ingestor.ingest({
      type: 'message.updated',
      properties: {
        info: {
          id: 'msg_existing_asst',
          sessionID: 'ses_existing',
          role: 'assistant',
          time: { created: 1 },
          tokens: { input: 1, output: 1 },
        },
      },
    });
    const result = await ingestor.ingest({
      type: 'session.idle',
      properties: { sessionID: 'ses_existing' },
    });
    assert.equal(result.turns.length, 0);
  });

  it('matches file-derived OpenCode records for the same completed session', async () => {
    const file = sessionFile('with-tool', 'ses_tool');
    const expected = await parseOpencodeSession(file, {
      contentMode: 'full',
      tokenizer: 'heuristic',
    });
    const ingestor = await createOpencodeStreamIngestor({
      contentMode: 'full',
      tokenizer: 'heuristic',
    });

    const result = await replayFixture(ingestor, 'with-tool', 'ses_tool');
    assert.equal(result.turns.length, expected.turns.length);
    assert.equal(result.turns[0]!.messageId, expected.turns[0]!.messageId);
    assert.deepEqual(result.turns[0]!.usage, expected.turns[0]!.usage);
    assert.deepEqual(
      result.turns[0]!.toolCalls.map((t) => [t.id, t.name, t.target]),
      expected.turns[0]!.toolCalls.map((t) => [t.id, t.name, t.target]),
    );
    assert.deepEqual(
      result.toolResultEvents.map((e) => [
        e.toolUseId,
        e.eventIndex,
        e.status,
        e.usageAttribution,
        e.usage?.input,
      ]),
      expected.toolResultEvents.map((e) => [
        e.toolUseId,
        e.eventIndex,
        e.status,
        e.usageAttribution,
        e.usage?.input,
      ]),
    );
    assert.equal(
      result.content.filter((c) => c.kind === 'tool_result').length,
      expected.content.filter((c) => c.kind === 'tool_result').length,
    );
  });
});

async function replayFixture(
  ingestor: Awaited<ReturnType<typeof createOpencodeStreamIngestor>>,
  fixture: string,
  sessionId: string,
) {
  const storage = path.join(FIXTURES, fixture, 'storage');
  const session = await readJson(path.join(storage, 'session', 'global', `${sessionId}.json`));
  await ingestor.ingest({ type: 'session.created', properties: { info: session } });

  const messageDir = path.join(storage, 'message', sessionId);
  const messages = await Promise.all(
    (await readdir(messageDir))
      .filter((name) => name.endsWith('.json'))
      .map((name) => readJson(path.join(messageDir, name))),
  );
  messages.sort((a, b) => Number(a.time?.created ?? 0) - Number(b.time?.created ?? 0));
  for (const message of messages) {
    await ingestor.ingest({ type: 'message.updated', properties: { info: message } });
    const partDir = path.join(storage, 'part', String(message.id));
    let partNames: string[];
    try {
      partNames = await readdir(partDir);
    } catch {
      partNames = [];
    }
    const parts = await Promise.all(
      partNames
        .filter((name) => name.endsWith('.json'))
        .map((name) => readJson(path.join(partDir, name))),
    );
    parts.sort((a, b) => String(a.id ?? '').localeCompare(String(b.id ?? '')));
    for (const part of parts) {
      await ingestor.ingest({ type: 'message.part.updated', properties: { part } });
    }
  }
  return ingestor.ingest({ type: 'session.idle', properties: { sessionID: sessionId } });
}

async function readJson(file: string): Promise<Record<string, any>> {
  return JSON.parse(await readFile(file, 'utf8')) as Record<string, any>;
}
