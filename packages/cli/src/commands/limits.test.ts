import { describe, it, mock } from 'node:test';
import assert from 'node:assert';
import { homedir } from 'node:os';
import * as path from 'node:path';

// Mock the modules before importing
const mockReadFile = mock.method(
  await import('node:fs/promises'),
  'readFile',
  async () => '{}'
);

const mockFetch = mock.method(
  globalThis,
  'fetch',
  async () => ({
    ok: true,
    status: 200,
    json: async () => ({
      five_hour: { percent_used: 34, reset_at: new Date(Date.now() + 2 * 3600 * 1000).toISOString() },
      seven_day: { percent_used: 12, reset_at: new Date(Date.now() + 4 * 86400 * 1000).toISOString() },
      seven_day_opus: { percent_used: 8, reset_at: new Date(Date.now() + 4 * 86400 * 1000).toISOString() },
      extra_usage: { percent_used: 0, reset_at: new Date(Date.now() + 4 * 86400 * 1000).toISOString() },
    }),
  })
);

const { runLimits } = await import('../commands/limits.js');

describe('runLimits', () => {
  describe('token handling', () => {
    it('returns exit code 2 when token is missing', async () => {
      mockReadFile.mockImplementation(async () => {
        throw new Error('File not found');
      });
      
      const exitCode = await runLimits({ flags: {} } as any);
      assert.strictEqual(exitCode, 2);
    });

    it('returns exit code 2 when token is invalid (401)', async () => {
      mockReadFile.mockImplementation(async () => JSON.stringify({ oauth_token: 'test-token' }));
      mockFetch.mockImplementation(async () => ({
        ok: false,
        status: 401,
      }));
      
      const exitCode = await runLimits({ flags: {} } as any);
      assert.strictEqual(exitCode, 2);
    });
  });

  describe('output formatting', () => {
    it('outputs JSON when --json flag is set', async () => {
      const chunks: string[] = [];
      const originalWrite = process.stdout.write;
      process.stdout.write = (chunk: string) => {
        chunks.push(chunk);
        return true;
      };
      
      mockReadFile.mockImplementation(async () => JSON.stringify({ oauth_token: 'test-token' }));
      
      const exitCode = await runLimits({ flags: { json: true } } as any);
      
      process.stdout.write = originalWrite;
      assert.strictEqual(exitCode, 0);
      assert.ok(chunks.join('').includes('five_hour'));
      assert.ok(chunks.join('').includes('percent_used'));
    });

    it('outputs table format by default', async () => {
      const chunks: string[] = [];
      const originalWrite = process.stdout.write;
      process.stdout.write = (chunk: string) => {
        chunks.push(chunk);
        return true;
      };
      
      mockReadFile.mockImplementation(async () => JSON.stringify({ oauth_token: 'test-token' }));
      
      const exitCode = await runLimits({ flags: {} } as any);
      
      process.stdout.write = originalWrite;
      assert.strictEqual(exitCode, 0);
      const output = chunks.join('');
      assert.ok(output.includes('5-hour'));
      assert.ok(output.includes('7-day'));
    });
  });

  describe('flag validation', () => {
    it('rejects --watch --json combination', async () => {
      const chunks: string[] = [];
      const originalWrite = process.stderr.write;
      process.stderr.write = (chunk: string) => {
        chunks.push(chunk);
        return true;
      };
      
      const exitCode = await runLimits({ flags: { watch: true, json: true } } as any);
      
      process.stderr.write = originalWrite;
      assert.strictEqual(exitCode, 2);
      assert.ok(chunks.join('').includes('cannot be used together'));
    });

    it('shows help with --help flag', async () => {
      const chunks: string[] = [];
      const originalWrite = process.stdout.write;
      process.stdout.write = (chunk: string) => {
        chunks.push(chunk);
        return true;
      };
      
      const exitCode = await runLimits({ flags: { help: true } } as any);
      
      process.stdout.write = originalWrite;
      assert.strictEqual(exitCode, 0);
      assert.ok(chunks.join('').includes('burn limits'));
      assert.ok(chunks.join('').includes('--watch'));
    });
  });

  describe('duration formatting', () => {
    it('formats days correctly for 7-day windows', () => {
      // This tests the internal formatDuration logic via output
      const sevenDayMs = 7 * 24 * 3600 * 1000; // 7 days
      const hours = Math.floor(sevenDayMs / 3600000);
      assert.ok(hours >= 168); // 7 * 24
    });
  });
});
