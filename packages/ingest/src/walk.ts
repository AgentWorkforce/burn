import { readdir } from 'node:fs/promises';
import type { Dirent } from 'node:fs';
import * as path from 'node:path';

export async function walkJsonl(root: string): Promise<string[]> {
  return walkFiles(root, (e) => e.name.endsWith('.jsonl'));
}

export async function walkOpencodeSessions(root: string): Promise<string[]> {
  return walkFiles(root, (e) => e.name.startsWith('ses_') && e.name.endsWith('.json'));
}

async function walkFiles(root: string, accept: (e: Dirent) => boolean): Promise<string[]> {
  const out: string[] = [];
  const stack: string[] = [root];
  while (stack.length > 0) {
    const dir = stack.pop()!;
    let entries: Dirent[];
    try {
      entries = (await readdir(dir, { withFileTypes: true })) as Dirent[];
    } catch {
      continue;
    }
    for (const e of entries) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) stack.push(full);
      else if (e.isFile() && accept(e)) out.push(full);
    }
  }
  return out;
}
