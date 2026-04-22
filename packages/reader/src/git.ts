import { readFileSync, statSync } from 'node:fs';
import * as path from 'node:path';

export interface ResolvedProject {
  project: string;
  projectKey?: string;
}

const cache = new Map<string, ResolvedProject>();

export function resolveProject(cwd: string): ResolvedProject {
  const cached = cache.get(cwd);
  if (cached) return cached;
  const result = resolveUncached(cwd);
  cache.set(cwd, result);
  return result;
}

export function __resetResolveProjectCacheForTesting(): void {
  cache.clear();
}

function resolveUncached(cwd: string): ResolvedProject {
  const gitDir = findGitDir(cwd);
  if (!gitDir) return { project: cwd };
  const configPath = path.join(gitDir, 'config');
  const text = tryRead(configPath);
  if (text === undefined) return { project: cwd };
  const config = parseGitConfig(text);
  const url = config['remote "origin"']?.['url'];
  if (!url) return { project: cwd };
  const projectKey = canonicalizeRemoteUrl(url);
  if (!projectKey) return { project: cwd };
  return { project: cwd, projectKey };
}

function findGitDir(startCwd: string): string | undefined {
  let dir = path.resolve(startCwd);
  for (let i = 0; i < 100; i++) {
    const candidate = path.join(dir, '.git');
    const st = tryStat(candidate);
    if (st) {
      if (st.isDirectory()) return candidate;
      if (st.isFile()) {
        const resolved = resolveWorktreeGitDir(candidate);
        if (resolved) return resolved;
      }
    }
    const parent = path.dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
  return undefined;
}

function resolveWorktreeGitDir(gitFile: string): string | undefined {
  const text = tryRead(gitFile);
  if (text === undefined) return undefined;
  const match = text.match(/^gitdir:\s*(.+?)\s*$/m);
  if (!match || !match[1]) return undefined;
  const rawGitdir = match[1];
  const gitdir = path.isAbsolute(rawGitdir)
    ? rawGitdir
    : path.resolve(path.dirname(gitFile), rawGitdir);
  const commondirFile = path.join(gitdir, 'commondir');
  const commondirText = tryRead(commondirFile);
  if (commondirText !== undefined) {
    const raw = commondirText.trim();
    if (raw.length > 0) {
      return path.isAbsolute(raw) ? raw : path.resolve(gitdir, raw);
    }
  }
  return gitdir;
}

export function parseGitConfig(text: string): Record<string, Record<string, string>> {
  const out: Record<string, Record<string, string>> = {};
  let current: Record<string, string> | undefined;
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.replace(/^[ \t]+/, '').replace(/[ \t]+$/, '');
    if (line.length === 0) continue;
    if (line.startsWith('#') || line.startsWith(';')) continue;
    if (line.startsWith('[') && line.endsWith(']')) {
      const name = sectionName(line.slice(1, -1));
      current = out[name] ?? (out[name] = {});
      continue;
    }
    if (!current) continue;
    const eq = line.indexOf('=');
    if (eq === -1) continue;
    const key = line.slice(0, eq).trim();
    const value = stripInlineComment(line.slice(eq + 1).trim());
    if (key.length === 0) continue;
    current[key] = value;
  }
  return out;
}

function sectionName(raw: string): string {
  const trimmed = raw.trim();
  const match = trimmed.match(/^([A-Za-z0-9._-]+)\s+"(.*)"$/);
  if (match) return `${match[1]} "${match[2]}"`;
  return trimmed;
}

function stripInlineComment(value: string): string {
  let out = '';
  let inQuotes = false;
  for (let i = 0; i < value.length; i++) {
    const ch = value[i];
    if (ch === '"') {
      inQuotes = !inQuotes;
      continue;
    }
    if (!inQuotes && (ch === '#' || ch === ';')) break;
    out += ch;
  }
  return out.trim();
}

export function canonicalizeRemoteUrl(url: string): string | undefined {
  const trimmed = url.trim();
  if (trimmed.length === 0) return undefined;

  const scp = trimmed.match(/^(?:[A-Za-z0-9_-]+)@([^:\s]+):(.+)$/);
  if (scp) {
    const host = scp[1]!.toLowerCase();
    const pathPart = stripDotGit(scp[2]!.replace(/^\/+/, '').replace(/\/+$/, ''));
    if (!pathPart) return undefined;
    return `${host}/${pathPart}`;
  }

  const schemeMatch = trimmed.match(/^([A-Za-z][A-Za-z0-9+.-]*):\/\/(.+)$/);
  if (schemeMatch) {
    const rest = schemeMatch[2]!;
    const atIdx = rest.indexOf('@');
    const afterAuth = atIdx === -1 ? rest : rest.slice(atIdx + 1);
    const slashIdx = afterAuth.indexOf('/');
    if (slashIdx === -1) return undefined;
    const hostPart = afterAuth.slice(0, slashIdx);
    const host = stripPort(hostPart).toLowerCase();
    if (!host) return undefined;
    const pathPart = stripDotGit(afterAuth.slice(slashIdx + 1).replace(/\/+$/, ''));
    if (!pathPart) return undefined;
    return `${host}/${pathPart}`;
  }

  return undefined;
}

function stripDotGit(p: string): string {
  return p.replace(/\.git$/, '');
}

function stripPort(host: string): string {
  const idx = host.indexOf(':');
  return idx === -1 ? host : host.slice(0, idx);
}

function tryRead(p: string): string | undefined {
  try {
    return readFileSync(p, 'utf8');
  } catch {
    return undefined;
  }
}

function tryStat(p: string): { isDirectory: () => boolean; isFile: () => boolean } | undefined {
  try {
    return statSync(p);
  } catch {
    return undefined;
  }
}
