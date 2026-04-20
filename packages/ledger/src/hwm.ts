import { mkdir, readFile, writeFile } from 'node:fs/promises';
import * as path from 'node:path';

import { hwmPath } from './paths.js';

export interface HwmEntry {
  lastMessageId: string;
  lastTs: string;
  mtimeMs: number;
}

export type HwmMap = Record<string, HwmEntry>;

export async function loadHwm(): Promise<HwmMap> {
  try {
    const raw = await readFile(hwmPath(), 'utf8');
    const parsed = JSON.parse(raw) as unknown;
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return parsed as HwmMap;
    }
    return {};
  } catch {
    return {};
  }
}

export async function saveHwm(map: HwmMap): Promise<void> {
  const p = hwmPath();
  await mkdir(path.dirname(p), { recursive: true });
  await writeFile(p, JSON.stringify(map, null, 2), 'utf8');
}
