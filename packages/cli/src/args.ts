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
      const body = arg.slice(2);
      // Support --foo=bar inline form so the issue spec `--patterns=a,b,c`
      // parses the way users expect. Bare --foo falls through to the
      // space-separated value / boolean path below.
      const eqIdx = body.indexOf('=');
      if (eqIdx > 0) {
        const name = body.slice(0, eqIdx);
        const value = body.slice(eqIdx + 1);
        if (name === 'tag') {
          const innerEq = value.indexOf('=');
          if (innerEq > 0) tags[value.slice(0, innerEq)] = value.slice(innerEq + 1);
        } else {
          flags[name] = value;
        }
        i++;
        continue;
      }
      const name = body;
      const next = argv[i + 1];
      if (name === 'tag' && next !== undefined) {
        const innerEq = next.indexOf('=');
        if (innerEq > 0) tags[next.slice(0, innerEq)] = next.slice(innerEq + 1);
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
