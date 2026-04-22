import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import * as path from 'node:path';

import { withLock } from './lock.js';
import { planUsagePath } from './paths.js';

interface PlanUsage {
  monthly: {
    claudePro: number;
    claudeMax: number;
    cursorPro: number;
    [key: string]: number;
  };
  daily: {
    claudePro: number;
    claudeMax: number;
    cursorPro: number;
    [key: string]: number;
  };
}

interface PlanUsageFile {
  usage: PlanUsage;
}

export async function loadPlanUsage(): Promise<PlanUsage> {
  try {
    const raw = await readFile(planUsagePath(), 'utf8');
    const parsed = JSON.parse(raw) as PlanUsageFile;
    if (parsed && typeof parsed === 'object' && parsed.usage && typeof parsed.usage === 'object') {
      return parsed.usage;
    }
  } catch {
    // missing or malformed: treat as empty
  }
  return {
    monthly: {
      claudePro: 0,
      claudeMax: 0,
      cursorPro: 0
    },
    daily: {
      claudePro: 0,
      claudeMax: 0,
      cursorPro: 0
    }
  };
}

export async function savePlanUsage(usage: PlanUsage): Promise<void> {
  const finalPath = planUsagePath();
  await mkdir(path.dirname(finalPath), { recursive: true });
  const payload: PlanUsageFile = { usage };
  const tmpPath = `${finalPath}.tmp`;
  await withLock('plan-usage', async () => {
    await writeFile(tmpPath, JSON.stringify(payload, null, 2), 'utf8');
    await rename(tmpPath, finalPath);
  });
}
