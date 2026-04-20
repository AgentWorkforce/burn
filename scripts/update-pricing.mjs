#!/usr/bin/env node
import { writeFile } from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.resolve(__dirname, '..', 'packages', 'analyze', 'pricing', 'models.dev.json');

const res = await fetch('https://models.dev/api.json');
if (!res.ok) {
  console.error(`fetch failed: ${res.status} ${res.statusText}`);
  process.exit(1);
}
const body = await res.text();
await writeFile(OUT, body, 'utf8');
console.log(`wrote ${OUT} (${body.length} bytes)`);
