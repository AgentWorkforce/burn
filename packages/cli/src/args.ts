export interface ParsedArgs {
  flags: Record<string, string | true>;
  tags: Record<string, string>;
  positional: string[];
  passthrough: string[];
}

export function parseArgs(argv: string[]): ParsedArgs {
  const flags: Record<string, string | true> = {};
  const tags: Record<string, string> = {};
  const positional: string[] = [];
  const passthrough: string[] = [];

  let i = 0;
  let reachedPassthrough = false;
  while (i < argv.length) {
    const arg = argv[i]!;
    if (arg === '--') {
      reachedPassthrough = true;
      i++;
      continue;
    }
    if (reachedPassthrough) {
      passthrough.push(arg);
      i++;
      continue;
    }
    if (arg.startsWith('--')) {
      const name = arg.slice(2);
      const next = argv[i + 1];
      if (name === 'tag' && next !== undefined) {
        const eq = next.indexOf('=');
        if (eq > 0) tags[next.slice(0, eq)] = next.slice(eq + 1);
        i += 2;
        continue;
      }
      if (next !== undefined && !next.startsWith('-')) {
        flags[name] = next;
        i += 2;
      } else {
        flags[name] = true;
        i++;
      }
    } else {
      positional.push(arg);
      i++;
    }
  }

  return { flags, tags, positional, passthrough };
}
