import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import {
  buildOpencodeEventHeaders,
  consumeOpencodeEventStream,
  isOpencodeIngestHint,
  parseSseEvent,
  resolveOpencodeEventUrl,
  splitSseFrames,
  type FetchResponseLike,
} from './opencode-stream.js';

describe('opencode event stream helpers', () => {
  it('resolves default and explicit OpenCode event URLs', () => {
    assert.equal(resolveOpencodeEventUrl(undefined, { env: {} }), 'http://127.0.0.1:4096/event');
    assert.equal(
      resolveOpencodeEventUrl(undefined, {
        global: true,
        env: { OPENCODE_SERVER_URL: 'http://localhost:5099' },
      }),
      'http://localhost:5099/global/event',
    );
    assert.equal(
      resolveOpencodeEventUrl('http://localhost:4096/event', { env: {} }),
      'http://localhost:4096/event',
    );
  });

  it('builds Basic auth headers from OpenCode server env vars', () => {
    const headers = buildOpencodeEventHeaders({
      OPENCODE_SERVER_USERNAME: 'alice',
      OPENCODE_SERVER_PASSWORD: 'secret',
    }, { lastEventId: 'evt_1' });
    assert.equal(headers['Accept'], 'text/event-stream');
    assert.equal(headers['Last-Event-ID'], 'evt_1');
    assert.equal(headers['Authorization'], 'Basic YWxpY2U6c2VjcmV0');
  });

  it('parses SSE frames and multiline data fields', () => {
    const split = splitSseFrames(
      ': hello\nid: 7\nevent: message\ndata: { "a": 1,\ndata: "b": 2 }\n\npartial',
    );
    assert.equal(split.frames.length, 1);
    assert.equal(split.rest, 'partial');
    const event = parseSseEvent(split.frames[0]!);
    assert.deepEqual(event, {
      id: '7',
      event: 'message',
      data: '{ "a": 1,\n"b": 2 }',
    });
  });

  it('recognizes project and global OpenCode events that should wake ingest', () => {
    assert.equal(isOpencodeIngestHint({ type: 'server.connected', properties: {} }), false);
    assert.equal(isOpencodeIngestHint({ type: 'message.part.updated', properties: {} }), true);
    assert.equal(
      isOpencodeIngestHint({
        directory: '/tmp/project',
        payload: { type: 'session.updated', properties: {} },
      }),
      true,
    );
  });

  it('consumes an SSE stream and calls onIngestHint for session/message events', async () => {
    let requestedUrl = '';
    let requestedLastEventId = '';
    const wakeTypes: string[] = [];
    const fetchImpl = async (
      url: string,
      init: { headers: Record<string, string> },
    ): Promise<FetchResponseLike> => {
      requestedUrl = url;
      requestedLastEventId = init.headers['Last-Event-ID'] ?? '';
      return responseFromChunks([
        'data: {"type":"server.connected","properties":{}}\n\n',
        'data: {"type":"message.part.updated","properties":{"sessionID":"ses_1"}}\n\n',
        'data: {"directory":"/tmp/project","payload":{"type":"session.updated","properties":{"sessionID":"ses_1"}}}\n\n',
      ]);
    };

    const report = await consumeOpencodeEventStream({
      baseUrl: 'http://localhost:4096',
      lastEventId: 'evt_prev',
      fetchImpl,
      env: {},
      onIngestHint(payload) {
        const rec = payload as { type?: string; payload?: { type?: string } };
        wakeTypes.push(rec.type ?? rec.payload?.type ?? '');
      },
    });

    assert.equal(requestedUrl, 'http://localhost:4096/event');
    assert.equal(requestedLastEventId, 'evt_prev');
    assert.deepEqual(report, { events: 3, wakeups: 2 });
    assert.deepEqual(wakeTypes, ['message.part.updated', 'session.updated']);
  });
});

function responseFromChunks(chunks: string[]): FetchResponseLike {
  const encoder = new TextEncoder();
  return {
    ok: true,
    status: 200,
    statusText: 'OK',
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
        controller.close();
      },
    }),
  };
}
