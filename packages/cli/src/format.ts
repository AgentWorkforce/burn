export function formatUsd(n: number): string {
  if (n === 0) return '$0.00';
  if (n < 0.01) return `$${n.toFixed(4)}`;
  if (n < 1) return `$${n.toFixed(3)}`;
  return `$${n.toFixed(2)}`;
}

export function formatInt(n: number): string {
  return n.toLocaleString('en-US');
}

export function table(rows: string[][]): string {
  if (rows.length === 0) return '';
  const widths: number[] = [];
  for (const row of rows) {
    row.forEach((cell, i) => {
      widths[i] = Math.max(widths[i] ?? 0, cell.length);
    });
  }
  return rows
    .map((row) => row.map((cell, i) => cell.padEnd(widths[i] ?? 0)).join('  ').trimEnd())
    .join('\n');
}

export function parseSinceArg(since: string): string {
  const m = /^(\d+)([hdwm])$/.exec(since);
  if (!m) {
    const d = new Date(since);
    if (Number.isNaN(d.getTime())) throw new Error(`invalid --since: ${since}`);
    return d.toISOString();
  }
  const n = parseInt(m[1]!, 10);
  const unit = m[2]!;
  const mult: Record<string, number> = {
    h: 60 * 60 * 1000,
    d: 24 * 60 * 60 * 1000,
    w: 7 * 24 * 60 * 60 * 1000,
    m: 30 * 24 * 60 * 60 * 1000,
  };
  const ms = n * (mult[unit] ?? 0);
  return new Date(Date.now() - ms).toISOString();
}
