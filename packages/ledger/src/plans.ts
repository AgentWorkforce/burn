export interface Plan {
  name: string;
  monthlyBudget: number;
  dailyBudget?: number;
  provider?: string;
  tier?: string;
}

export const DEFAULT_PLANS: Record<string, Plan> = {
  claudePro: {
    name: 'Claude Pro',
    monthlyBudget: 20,
    provider: 'anthropic',
    tier: 'pro'
  },
  claudeMax: {
    name: 'Claude Max',
    monthlyBudget: 200,
    provider: 'anthropic',
    tier: 'max'
  },
  cursorPro: {
    name: 'Cursor Pro',
    monthlyBudget: 20,
    provider: 'cursor',
    tier: 'pro'
  }
};

export function loadCustomPlans(): Record<string, Plan> {
  try {
    const fs = require('node:fs');
    const path = require('node:path');
    const home = process.env.RELAYBURN_HOME || path.join(process.env.HOME || '', '.relayburn');
    const plansPath = path.join(home, 'plans.json');
    if (fs.existsSync(plansPath)) {
      const raw = fs.readFileSync(plansPath, 'utf8');
      return JSON.parse(raw) as Record<string, Plan>;
    }
  } catch {
    // ignore errors
  }
  return {};
}
