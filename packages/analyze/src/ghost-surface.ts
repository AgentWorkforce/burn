// Cross-harness ghost-user-installed-surface detector. Closes #166.
//
// User-installed surface files (agents, skills, commands, prompts, rules,
// memories) ride in every session's system prompt. When the user has
// authored a file but the agent never invokes it, the file is dead weight on
// every API call — the same fixed token cost paid on every session for zero
// utility. This detector enumerates those files per harness and
// cross-references basenames against observed tool-call / agent / command /
// prompt names in the user's session history.
//
// Per-harness adapters (Claude / Codex / OpenCode) keep the logic
// declarative. Adding a new harness is one new `GhostSurfaceAdapter` plus a
// registry entry. The top-level `detectGhostSurface` orchestrator runs each
// adapter on its own filesystem surface and folds the results into a single
// `GhostFinding[]`.
//
// OpenCode dedup vs. #54: the OpenCode catalog-bloat detector
// (`SystemPromptTax` in `patterns.ts`) already attributes the cost of the
// declared skill catalog as a per-session fixed tax. To avoid
// double-counting, ghost candidates whose basenames appear in the OpenCode
// declared catalog set are still surfaced (so the user knows what to remove)
// but emitted with `cost: 0`. Project-level skills and custom commands not
// in the declared catalog are costed normally.
//
// #172 follow-up — slash-command-style invocations (e.g. a user typing
// `/openspec-archive` in the UI) are NOT recorded as tool calls and so
// won't appear in `observedToolNamesBySource`. To close the gap without a
// breaking change, each adapter optionally implements `observedNames(inputs)`:
// the orchestrator unions whatever extra names that returns into the
// observed-names set before filtering candidates. The Claude and Codex
// adapters use that hook to mine `userTurnTextBySession` for slash-command
// markers (`<command-name>` blocks for Claude, literal `/<basename>` matches
// for Codex). The map is source-keyed first (`Map<SourceKind, Map<string,
// string[]>>`) so each adapter only sees its own source's text — without
// that scoping, a Claude `<command-name>/foo</command-name>` marker would
// de-ghost an identically-named Codex prompt because the Codex miner's
// word-boundary checks pass against the surrounding XML angle brackets.
// When `userTurnTextBySession` is undefined or the adapter's source has
// no entries (content store is `off` / sidecar pruned for every session
// of that source) the hook is a no-op and the detector falls back to v1
// behaviour.

import * as fs from 'node:fs';
import * as path from 'node:path';
import * as os from 'node:os';

import type { SourceKind } from '@relayburn/reader';

import type { WasteAction, WasteFinding } from './findings.js';

export type GhostFindingKind =
  | 'ghost-agent'
  | 'ghost-skill'
  | 'ghost-command'
  | 'ghost-prompt'
  | 'ghost-rule'
  | 'ghost-memory';

// The narrow per-detector struct (mirrors the `RetryLoop`/`FailureRun`/...
// pattern in `patterns.ts`). The unified `WasteFinding` envelope is produced
// downstream by `ghostSurfaceToFinding`.
export interface GhostSurfaceFinding {
  source: SourceKind;
  kind: GhostFindingKind;
  // Absolute path to the file on disk. The suggested fix uses this verbatim.
  path: string;
  // Approximate token cost of the file, computed as Math.ceil(byteLen / 4).
  // Mirrors the cheap heuristic used in `UserTurnBlock.approxTokens`.
  sizeTokens: number;
  // Cumulative cost across the window: sizeTokens × sessionCountInWindow ×
  // dollar-per-token rate. Drives the CLI's ghost-surface table column. Zero
  // when the entry was already counted by the OpenCode catalog-bloat detector
  // (#54) — see `countedByCatalogBloat`.
  cost: number;
  // Per-session cost: sizeTokens × dollar-per-token rate. This is what feeds
  // severity classification and `estimatedSavings.usdPerSession` in the
  // unified `WasteFinding` envelope, so a low-traffic ghost isn't ranked as
  // `high` purely because it rode in many sessions. Zero when
  // `countedByCatalogBloat` is true.
  costPerSession: number;
  // Number of sessions observed in the lookback window for this source.
  // Reported alongside cost so the user can sanity-check the multiplier.
  sessionCount: number;
  // True iff this entry's cost is also represented in the OpenCode
  // catalog-bloat tax (`SystemPromptTax`). Emitted with `cost: 0` to avoid
  // double-counting the dollars but still surfaced so the user knows which
  // catalog entry to remove.
  countedByCatalogBloat?: boolean;
}

// One file enumerated from a harness surface, before cross-referencing
// observed names. Adapters emit `GhostCandidate[]`; the orchestrator filters
// down to ghosts.
export interface GhostCandidate {
  source: SourceKind;
  kind: GhostFindingKind;
  path: string;
  basename: string;
  sizeTokens: number;
  countedByCatalogBloat?: boolean;
}

export interface GhostSurfaceInputs {
  // sessionId-keyed sets of *normalized invocation names* observed in the
  // ledger window for this source. The adapter compares file basenames
  // against the union of every set for its own source.
  //
  // For Claude: union of `Subagent.subagentType`, slash-command names (when
  //   #172 lands), and tool-call names of the surface kind (e.g. an Agent
  //   tool call's `subagent_type`).
  // For Codex: same shape. Slash-command-style prompt invocations are not
  //   recorded as tool calls — see #172.
  // For OpenCode: skill names from `ToolCall.skillName` plus subagent
  //   names plus custom-command names (when surfaced).
  observedNamesBySource: Map<SourceKind, Set<string>>;

  // Per-source session count in the ledger window. Drives the cost
  // multiplier (a ghost file rides in every one of those sessions).
  sessionCountBySource: Map<SourceKind, number>;

  // A flat dollar-per-token rate used for the cost estimate. The detector
  // doesn't know which model was used per session (a user could mix), so it
  // uses one blended rate for the report. The CLI orchestrator passes the
  // input-rate of a representative model (typically the most-used model in
  // the window, or the cheapest input rate if no usage is available).
  dollarPerToken: number;

  // Optional adapter overrides. When undefined, adapters use the default
  // home-relative paths (`~/.claude`, `~/.codex`).
  claudeHome?: string;
  codexHome?: string;
  // OpenCode is project-relative; an empty array runs the adapter against
  // the current working directory.
  opencodeProjects?: string[];

  // Per-source, per-session list of raw user-turn text strings observed in
  // the ledger window. The Claude and Codex adapters' optional
  // `observedNames` hook consumes its own source's inner map to mine
  // slash-command-style invocations that don't surface as tool calls
  // (#172). The CLI populates this from the per-session content sidecar
  // when content store is `full`; sessions whose sidecar is empty (hash-only
  // / off / pruned) are simply absent from the inner map and the detector
  // falls back to v1 (tool-call only) behaviour for that source.
  //
  // The map is keyed by `SourceKind` first so an adapter only ever sees
  // its own source's text. Without that scoping, a Claude
  // `<command-name>/foo</command-name>` marker would de-ghost an
  // identically-named Codex prompt (and vice versa) because the Codex
  // miner does a literal `/<stem>` match that passes word-boundary checks
  // against the surrounding `<command-name>` XML angle brackets.
  userTurnTextBySession?: Map<SourceKind, Map<string, string[]>> | undefined;
}

export interface GhostSurfaceAdapter {
  source: SourceKind;
  // Enumerate user-installed surface files for this harness. Implementations
  // are filesystem-bound; the orchestrator tolerates ENOENT and returns an
  // empty list (the user simply hasn't installed the surface in question).
  enumerate(inputs: GhostSurfaceInputs): Promise<GhostCandidate[]>;
  // Optional per-adapter observation pass. Returns extra observed names
  // (e.g. mined from `userTurnTextBySession`) that should be unioned into
  // the cross-referenced set before deciding which candidates are ghosts.
  // Returning an empty set is a no-op; the orchestrator never calls this
  // hook when `userTurnTextBySession` is undefined. The hook is opt-in so
  // adapters that don't have a slash-command notion (OpenCode) can omit it.
  observedNames?(
    inputs: GhostSurfaceInputs,
    candidates: ReadonlyArray<GhostCandidate>,
  ): Set<string>;
}

export interface DetectGhostSurfaceOptions {
  // Custom adapter list — primarily for tests. Production callers should
  // omit this and get the default `[claudeGhostAdapter, codexGhostAdapter,
  // opencodeGhostAdapter]` registry.
  adapters?: GhostSurfaceAdapter[];
}

// Token-byte heuristic. Matches `UserTurnBlock.approxTokens`
// (`Math.ceil(byteLen / 4)`) so reports are consistent across detectors.
const APPROX_BYTES_PER_TOKEN = 4;

function approxTokensFromBytes(byteLen: number): number {
  return Math.ceil(byteLen / APPROX_BYTES_PER_TOKEN);
}

// Strip a file extension off a basename. Skill / prompt / agent files are
// matched by their stem against observed invocation names — `foo.md` is the
// `foo` agent or the `foo` skill. Multi-dot stems are reduced to the last
// extension (`foo.tar.md` → `foo.tar`); the lookup set the orchestrator
// builds is stem-keyed too.
function stripExtension(basename: string): string {
  const lastDot = basename.lastIndexOf('.');
  if (lastDot <= 0) return basename;
  return basename.slice(0, lastDot);
}

// Return both the raw stem and a lowercase variant. Some harnesses (Codex
// `read_file` vs Claude `Read`) normalize tool names with mixed case; we
// match case-insensitively to be forgiving without losing the original
// path on the report.
function namesForLookup(basename: string): string[] {
  const stem = stripExtension(basename);
  const lower = stem.toLowerCase();
  return stem === lower ? [stem] : [stem, lower];
}

// Walk a directory non-recursively (one level deep is the documented surface
// for Claude / Codex). Returns absolute paths to regular files matching
// `predicate`; returns `[]` when the directory doesn't exist. We intentionally
// don't recurse — nested directories are out of scope for the issue and would
// invite picking up README/test fixtures.
function listDirFiles(
  dir: string,
  predicate: (basename: string) => boolean,
): { path: string; basename: string; size: number }[] {
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (err) {
    const e = err as NodeJS.ErrnoException;
    if (e.code === 'ENOENT' || e.code === 'ENOTDIR') return [];
    throw err;
  }
  const out: { path: string; basename: string; size: number }[] = [];
  for (const entry of entries) {
    if (!entry.isFile()) continue;
    if (!predicate(entry.name)) continue;
    const full = path.join(dir, entry.name);
    let size = 0;
    try {
      size = fs.statSync(full).size;
    } catch {
      continue;
    }
    out.push({ path: full, basename: entry.name, size });
  }
  return out;
}

function isMarkdown(name: string): boolean {
  return name.endsWith('.md') || name.endsWith('.markdown');
}

// Treats any plain-text-looking file as part of the surface. Codex memories /
// rules don't have a fixed extension, but the convention is `.md` or no
// extension. We keep this loose so users with odd filenames aren't silently
// excluded — but we do skip dotfiles and obvious binary suffixes.
function isPlainTextSurface(name: string): boolean {
  if (name.startsWith('.')) return false;
  // Conservative deny-list: anything that's clearly not a prompt/skill/rule.
  if (/\.(jsonl?|ya?ml|toml|lock|log|tsbuildinfo|png|jpg|jpeg|gif|webp|pdf|zip|tar|gz)$/i.test(name)) {
    return false;
  }
  return true;
}

// -- Slash-command observation (#172) ----------------------------------------

// Claude inlines slash-command expansion into the user message wrapped in a
// `<command-name>...</command-name>` tag. The inner text is either `/foo`
// or `foo` depending on Claude's UI version; we accept both. Multi-line
// content / leading-whitespace / mixed casing are tolerated. We extract the
// raw stem (`foo`) and let the orchestrator's case-insensitive compare do
// the rest.
const CLAUDE_COMMAND_NAME_RE = /<command-name>\s*\/?([\w./:-]+?)\s*<\/command-name>/gi;

export function mineClaudeCommandNames(
  userTurnTextBySession: Map<string, string[]> | undefined,
): Set<string> {
  const out = new Set<string>();
  if (!userTurnTextBySession || userTurnTextBySession.size === 0) return out;
  for (const texts of userTurnTextBySession.values()) {
    for (const text of texts) {
      if (!text) continue;
      // String#matchAll requires a /g regex; reset lastIndex isn't an issue
      // because we instantiate a new RegExp each call below.
      const re = new RegExp(CLAUDE_COMMAND_NAME_RE.source, 'gi');
      for (const match of text.matchAll(re)) {
        const raw = match[1];
        if (!raw) continue;
        // Strip a trailing arg list (`foo args` is `<command-name>foo args`)
        // — Claude has historically wrapped just the name, but a defensive
        // split keeps us forward-compatible if that changes.
        const head = raw.split(/[\s]/, 1)[0];
        if (head) out.add(head);
      }
    }
  }
  return out;
}

// Codex slash-commands expand verbatim before sending, so there's no marker
// to anchor on. We do a literal `/<stem>` match against the on-disk Codex
// prompt basenames — i.e. the candidate set the adapter just enumerated.
// Constraining the search to known stems prevents false positives from
// arbitrary `/usr/local/...` paths or `/path/to/file` substrings in user
// text. The match is anchored on a non-word character (or string start) on
// the left to avoid `/foo` matching `/foofoo`, but the right-hand boundary
// is loose so `/foo bar` and `/foo\n` both match.
//
// Limitation: a prompt invoked entirely without typing `/<basename>` (e.g.
// the user pastes the prompt body manually) won't be recognised. That's
// the documented false-negative — see the inline note on `codexGhostAdapter`.
export function mineCodexSlashInvocations(
  userTurnTextBySession: Map<string, string[]> | undefined,
  candidates: ReadonlyArray<GhostCandidate>,
): Set<string> {
  const out = new Set<string>();
  if (!userTurnTextBySession || userTurnTextBySession.size === 0) return out;
  // Build a stem -> raw-name lookup so we can return the original basename
  // form. Stems are lower-cased for the search but emitted as the original
  // candidate basename's stem (the orchestrator does case-insensitive
  // compare anyway, so this is mostly a courtesy for downstream consumers).
  const stems = new Map<string, string>();
  for (const cand of candidates) {
    const stem = stripExtension(cand.basename);
    if (!stem) continue;
    stems.set(stem.toLowerCase(), stem);
  }
  if (stems.size === 0) return out;
  for (const texts of userTurnTextBySession.values()) {
    for (const text of texts) {
      if (!text) continue;
      const lower = text.toLowerCase();
      for (const [stemLower, stemOriginal] of stems) {
        // Find every occurrence so a single user message that mentions
        // `/foo` and `/bar` resolves both.
        let from = 0;
        while (from <= lower.length) {
          const idx = lower.indexOf(`/${stemLower}`, from);
          if (idx < 0) break;
          // Left boundary: character before the slash must NOT be a word
          // character — otherwise `https://example.com/foo` and similar
          // path-y strings would falsely match. (`/` itself is fine as a
          // boundary; `//foo` is unusual but not a code path we care about.)
          if (idx > 0) {
            const left = lower.charCodeAt(idx - 1);
            if (isWordCharCode(left)) {
              from = idx + 1;
              continue;
            }
          }
          // Right boundary: character after the stem must NOT be a word
          // character or a hyphen. This is what stops `/foo` from matching
          // `/foobar` (or `/foo-bar` when only `/foo` is installed). A
          // hyphen is excluded because Codex prompt names commonly use
          // hyphens — `/openspec-apply` should not de-ghost a stem named
          // `openspec`.
          const after = idx + 1 + stemLower.length;
          if (after < lower.length) {
            const right = lower.charCodeAt(after);
            if (isWordCharCode(right) || right === 0x2d /* '-' */) {
              from = idx + 1;
              continue;
            }
          }
          out.add(stemOriginal);
          from = after;
          break; // matched; move on to the next text body
        }
      }
    }
  }
  return out;
}

function isWordCharCode(c: number): boolean {
  // [A-Za-z0-9_]. Hyphen is intentionally excluded — see the right-boundary
  // note in `mineCodexSlashInvocations`.
  return (
    (c >= 0x30 && c <= 0x39) ||
    (c >= 0x41 && c <= 0x5a) ||
    (c >= 0x61 && c <= 0x7a) ||
    c === 0x5f
  );
}

// -- Claude adapter -----------------------------------------------------------
//
// Surfaces under `~/.claude/{agents,skills,commands}/`. Each file's stem is
// the invocation name — an agent at `~/.claude/agents/foo.md` is invoked as
// `subagent_type: 'foo'` in a Task tool call.
//
// NOTE (#172): slash-command-style invocations (e.g. the user typing `/foo`
// in the Claude UI) are NOT recorded as tool calls in the session log.
// `observedNames` augments the observed-names set by mining
// `userTurnTextBySession` for the `<command-name>...</command-name>`
// markers that Claude inlines into the user message when a slash command
// expands. Both `<command-name>/foo</command-name>` and bare
// `<command-name>foo</command-name>` are recognised. When the content
// sidecar is unavailable (`content.store=off` or pruned), the hook
// observes nothing and the detector falls back to v1 (tool-call only)
// behaviour — meaning a slash-only command file may still be reported as
// a ghost on those sessions.
export const claudeGhostAdapter: GhostSurfaceAdapter = {
  source: 'claude-code',
  async enumerate(inputs) {
    const home = inputs.claudeHome ?? path.join(os.homedir(), '.claude');
    const out: GhostCandidate[] = [];
    const surfaces: { kind: GhostFindingKind; subdir: string }[] = [
      { kind: 'ghost-agent', subdir: 'agents' },
      { kind: 'ghost-skill', subdir: 'skills' },
      { kind: 'ghost-command', subdir: 'commands' },
    ];
    for (const surface of surfaces) {
      const dir = path.join(home, surface.subdir);
      for (const file of listDirFiles(dir, isMarkdown)) {
        out.push({
          source: 'claude-code',
          kind: surface.kind,
          path: file.path,
          basename: file.basename,
          sizeTokens: approxTokensFromBytes(file.size),
        });
      }
    }
    return out;
  },
  observedNames(inputs) {
    return mineClaudeCommandNames(inputs.userTurnTextBySession?.get('claude-code'));
  },
};

// -- Codex adapter ------------------------------------------------------------
//
// Surfaces under `~/.codex/{prompts,skills,rules,memories}/`. Same
// stem-as-invocation-name convention as Claude.
//
// NOTE (#172): Codex slash-commands expand inline before sending — the
// prompt body is prepended verbatim to the user's input, with no
// `<command-name>` marker. `observedNames` mines `userTurnTextBySession`
// for literal `/<basename>` matches against the on-disk prompt stems.
// This is intentionally narrow: matching against the prompt's first line
// or heading would produce false positives whenever the user happens to
// quote that text. The trade-off is a known false-negative for prompts
// the user invoked but never typed `/<basename>` for (e.g. when the user
// pastes the prompt body manually). When the content sidecar is
// unavailable (`content.store=off` or pruned), the hook observes nothing
// and the detector falls back to v1 (tool-call only) behaviour.
export const codexGhostAdapter: GhostSurfaceAdapter = {
  source: 'codex',
  async enumerate(inputs) {
    const home = inputs.codexHome ?? path.join(os.homedir(), '.codex');
    const out: GhostCandidate[] = [];
    const surfaces: { kind: GhostFindingKind; subdir: string; predicate: (name: string) => boolean }[] = [
      { kind: 'ghost-prompt', subdir: 'prompts', predicate: isPlainTextSurface },
      { kind: 'ghost-skill', subdir: 'skills', predicate: isPlainTextSurface },
      { kind: 'ghost-rule', subdir: 'rules', predicate: isPlainTextSurface },
      { kind: 'ghost-memory', subdir: 'memories', predicate: isPlainTextSurface },
    ];
    for (const surface of surfaces) {
      const dir = path.join(home, surface.subdir);
      for (const file of listDirFiles(dir, surface.predicate)) {
        out.push({
          source: 'codex',
          kind: surface.kind,
          path: file.path,
          basename: file.basename,
          sizeTokens: approxTokensFromBytes(file.size),
        });
      }
    }
    return out;
  },
  observedNames(inputs, candidates) {
    return mineCodexSlashInvocations(inputs.userTurnTextBySession?.get('codex'), candidates);
  },
};

// -- OpenCode adapter ---------------------------------------------------------
//
// OpenCode is project-relative, not home-relative. The detector inspects:
//   - `<project>/opencode.json` for declared skills + custom commands
//   - `<project>/.opencode/skills/` (or `<project>/skills/`) for project skills
//
// `opencodeProjects` is the list of project roots to scan. When the input is
// undefined or empty, the adapter scans the current working directory.
//
// Catalog-bloat dedup (#54): every skill *declared in `opencode.json`* (i.e.
// part of the OpenCode skill catalog that ships in the system prompt on
// every API call) is marked with `countedByCatalogBloat: true`. The
// `SystemPromptTax` detector already attributes those tokens as part of the
// per-session fixed prefix tax — so we surface the entry (the user wants to
// know which catalog skills to remove) but emit it with `cost: 0` to avoid
// double-counting the dollars. Project-level skills not in the declared
// catalog and custom commands fall through normally.
export const opencodeGhostAdapter: GhostSurfaceAdapter = {
  source: 'opencode',
  async enumerate(inputs) {
    const projects =
      inputs.opencodeProjects && inputs.opencodeProjects.length > 0
        ? inputs.opencodeProjects
        : [process.cwd()];
    const out: GhostCandidate[] = [];
    for (const project of projects) {
      out.push(...enumerateOpenCodeProject(project));
    }
    return out;
  },
};

function enumerateOpenCodeProject(project: string): GhostCandidate[] {
  const out: GhostCandidate[] = [];
  const declaredSkills = new Set<string>();
  const declaredCommands: { name: string; sizeTokens: number; path: string }[] = [];

  const configPath = path.join(project, 'opencode.json');
  let configRaw: string | undefined;
  try {
    configRaw = fs.readFileSync(configPath, 'utf8');
  } catch (err) {
    const e = err as NodeJS.ErrnoException;
    if (e.code !== 'ENOENT') throw err;
  }
  if (configRaw) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(configRaw);
    } catch {
      // Malformed config — skip catalog-bloat dedup but continue with the
      // project skills folder. We don't fail the whole detector on a JSON
      // parse error because the user may still have project skills.
      parsed = undefined;
    }
    if (parsed && typeof parsed === 'object') {
      const cfg = parsed as Record<string, unknown>;
      // Declared catalog skills — `skills: { foo: {...}, bar: {...} }` or
      // `skills: ['foo', 'bar']`. Both shapes are observed in the wild.
      const skills = cfg['skills'];
      if (skills && typeof skills === 'object' && !Array.isArray(skills)) {
        for (const name of Object.keys(skills as Record<string, unknown>)) {
          declaredSkills.add(name);
        }
      } else if (Array.isArray(skills)) {
        for (const s of skills) if (typeof s === 'string') declaredSkills.add(s);
      }
      // Custom commands — `commands: { foo: { description, prompt } }`.
      // Ride in the system prompt the same as a Claude command file but live
      // inside the project's opencode.json. We size them by their JSON
      // serialization length as a stand-in for "how much they bloat the
      // prompt".
      const commands = cfg['commands'];
      if (commands && typeof commands === 'object' && !Array.isArray(commands)) {
        for (const [name, val] of Object.entries(commands as Record<string, unknown>)) {
          const serialized = JSON.stringify(val ?? {});
          declaredCommands.push({
            name,
            sizeTokens: approxTokensFromBytes(serialized.length),
            // The "path" for an opencode.json command points at the config
            // file with a JSON-pointer fragment, so the suggested fix can
            // hand the user a real location. We use a fragment because
            // opencode.json is a single shared file.
            path: `${configPath}#/commands/${name}`,
          });
        }
      }
    }
  }

  // Declared catalog skills — emit with countedByCatalogBloat. Path points
  // at the config entry; the user removes it by editing opencode.json.
  for (const name of declaredSkills) {
    out.push({
      source: 'opencode',
      kind: 'ghost-skill',
      path: `${configPath}#/skills/${name}`,
      basename: name,
      // Declared-skill size lives inside opencode.json; we don't separate it
      // from the catalog bloat number, so report 0 tokens here. The
      // catalog-bloat detector covers the cost.
      sizeTokens: 0,
      countedByCatalogBloat: true,
    });
  }

  // Custom commands.
  for (const cmd of declaredCommands) {
    out.push({
      source: 'opencode',
      kind: 'ghost-command',
      path: cmd.path,
      basename: cmd.name,
      sizeTokens: cmd.sizeTokens,
    });
  }

  // Project skills folder. Try `.opencode/skills` first, then `skills` as a
  // fallback (different OpenCode templates lay these out differently).
  const skillDirs = [
    path.join(project, '.opencode', 'skills'),
    path.join(project, 'skills'),
  ];
  for (const dir of skillDirs) {
    for (const file of listDirFiles(dir, isMarkdown)) {
      out.push({
        source: 'opencode',
        kind: 'ghost-skill',
        path: file.path,
        basename: file.basename,
        sizeTokens: approxTokensFromBytes(file.size),
        // Project-folder skills are not part of opencode.json catalog, so
        // they aren't covered by the SystemPromptTax detector — fall
        // through and cost normally.
      });
    }
  }

  return out;
}

export const DEFAULT_GHOST_ADAPTERS: GhostSurfaceAdapter[] = [
  claudeGhostAdapter,
  codexGhostAdapter,
  opencodeGhostAdapter,
];

// Top-level orchestrator. Runs each adapter on its own filesystem surface,
// cross-references basenames against observed-names, and folds the matched
// ghosts into a single `GhostSurfaceFinding[]`.
//
// Per-source dedup of the observed set: an adapter only checks its own
// source's observed names. A Claude agent named `code-reviewer` is matched
// against Claude's observed-names set, not Codex's — different harnesses
// have different surfaces and inflicting cross-harness matching would mask
// real ghosts.
export async function detectGhostSurface(
  inputs: GhostSurfaceInputs,
  options: DetectGhostSurfaceOptions = {},
): Promise<GhostSurfaceFinding[]> {
  const adapters = options.adapters ?? DEFAULT_GHOST_ADAPTERS;
  const out: GhostSurfaceFinding[] = [];
  for (const adapter of adapters) {
    const candidates = await adapter.enumerate(inputs);
    const observedRaw = inputs.observedNamesBySource.get(adapter.source) ?? new Set<string>();
    // Build a lower-cased lookup set so comparisons are case-insensitive
    // without forcing callers to pre-normalize their observed-names input.
    const observedLower = new Set<string>();
    for (const name of observedRaw) observedLower.add(name.toLowerCase());
    // Adapter-local observation pass (#172). Slash-command invocations
    // mined from `userTurnTextBySession` are unioned in here so they
    // de-ghost a basename without leaking into other adapters' observed
    // sets. Only invoked when this adapter's source has at least one
    // session's worth of text — without that gate, a Codex-only run would
    // still call `claudeGhostAdapter.observedNames` with an undefined map
    // and the hook would have to defensively no-op. Per-source scoping
    // also prevents cross-harness contamination: Claude's `<command-name>`
    // markers are not exposed to the Codex miner, and Codex's literal
    // `/<stem>` matches are not exposed to the Claude miner.
    const sourceTexts = inputs.userTurnTextBySession?.get(adapter.source);
    if (
      adapter.observedNames !== undefined &&
      sourceTexts !== undefined &&
      sourceTexts.size > 0
    ) {
      const extra = adapter.observedNames(inputs, candidates);
      for (const name of extra) observedLower.add(name.toLowerCase());
    }
    const sessionCount = inputs.sessionCountBySource.get(adapter.source) ?? 0;
    for (const cand of candidates) {
      const lookups = namesForLookup(cand.basename);
      const isInvoked = lookups.some((n) => observedLower.has(n.toLowerCase()));
      if (isInvoked) continue;
      const costPerSession = cand.countedByCatalogBloat
        ? 0
        : cand.sizeTokens * inputs.dollarPerToken;
      const cost = cand.countedByCatalogBloat
        ? 0
        : costPerSession * sessionCount;
      const finding: GhostSurfaceFinding = {
        source: cand.source,
        kind: cand.kind,
        path: cand.path,
        sizeTokens: cand.sizeTokens,
        cost,
        costPerSession,
        sessionCount,
      };
      if (cand.countedByCatalogBloat !== undefined) {
        finding.countedByCatalogBloat = cand.countedByCatalogBloat;
      }
      out.push(finding);
    }
  }
  // Sort by cost descending so the worst offenders surface first; ties go to
  // size, then path for stability.
  out.sort((a, b) => {
    if (b.cost !== a.cost) return b.cost - a.cost;
    if (b.sizeTokens !== a.sizeTokens) return b.sizeTokens - a.sizeTokens;
    return a.path.localeCompare(b.path);
  });
  return out;
}

// -- Finding envelope adapter -------------------------------------------------

const SEVERITY_HIGH_USD = 0.5;
const SEVERITY_WARN_USD = 0.05;

function fmtUsd(n: number): string {
  return `$${n.toFixed(4)}`;
}

function severityFromUsd(usd: number): WasteFinding['severity'] {
  if (usd >= SEVERITY_HIGH_USD) return 'high';
  if (usd >= SEVERITY_WARN_USD) return 'warn';
  return 'info';
}

// Default archive directory the suggested-fix command moves a ghost into.
// `~/.relayburn/ghost-archive/` keeps the move local to the relayburn data
// dir so the user can reverse the action without polluting the harness's
// own home directory. The CLI may override this when a future
// `burn waste --apply` flag wants to confirm the destination.
function defaultArchiveDir(): string {
  return path.join(os.homedir(), '.relayburn', 'ghost-archive');
}

export interface GhostSurfaceFindingOptions {
  archiveDir?: string;
}

// POSIX shell single-quote escape: wrap the string in single quotes and
// replace each embedded `'` with `'\''`. Safe for paths with spaces, `$`,
// backticks, or other shell metacharacters.
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

// Synthetic OpenCode paths look like `<configPath>#/skills/<name>` or
// `<configPath>#/commands/<name>` — they aren't real filesystem paths so a
// `mv` would fail. Detect them and emit a `paste`-style instruction instead.
function splitSyntheticPath(p: string): { file: string; pointer: string } | null {
  const hash = p.indexOf('#');
  if (hash < 0) return null;
  return { file: p.slice(0, hash), pointer: p.slice(hash + 1) };
}

export function ghostSurfaceToFinding(
  ghost: GhostSurfaceFinding,
  options: GhostSurfaceFindingOptions = {},
): WasteFinding {
  const archiveDir = options.archiveDir ?? defaultArchiveDir();
  const synthetic = splitSyntheticPath(ghost.path);
  const action: WasteAction = synthetic
    ? {
        type: 'paste',
        label: `Remove ghost ${ghost.kind} from ${path.basename(synthetic.file)}`,
        text: `Edit ${synthetic.file} and remove the entry at ${synthetic.pointer}.`,
      }
    : {
        type: 'command',
        label: `Archive ghost ${ghost.kind}`,
        text: `mkdir -p ${shellQuote(archiveDir)} && mv ${shellQuote(ghost.path)} ${shellQuote(archiveDir + '/')}`,
      };
  const perSessionUsd = ghost.costPerSession;
  const severity = severityFromUsd(perSessionUsd);
  // Ghost findings have no per-session id. Use the path as a stable key so
  // the unified finding renderer (which keys by sessionId) doesn't collide
  // multiple ghost findings; severity ranking still works because we ranked
  // by usdPerSession in `sortFindings`.
  const sessionId = `ghost:${ghost.path}`;
  const dedupNote = ghost.countedByCatalogBloat
    ? ' Cost is reported as $0 here because the OpenCode catalog-bloat detector already attributes this entry — see `burn waste --patterns opencode-system-prompt`.'
    : '';
  const sessionsClause =
    ghost.sessionCount > 0
      ? ` Observed across ${ghost.sessionCount} ${ghost.source} session(s) in the lookback window.`
      : '';
  return {
    kind: ghost.kind,
    severity,
    sessionId,
    title: `Ghost ${ghost.kind.replace('ghost-', '')}: ${path.basename(ghost.path).split('#')[0] ?? ghost.path} (${ghost.source})`,
    detail:
      `${ghost.path} is part of the user-installed ${ghost.source} surface ` +
      `(~${ghost.sizeTokens.toLocaleString()} tokens) but its basename was never invoked ` +
      `as a tool / agent / command / prompt in the observed window.${sessionsClause} ` +
      `Per-session cost ${fmtUsd(perSessionUsd)}; cumulative ${fmtUsd(ghost.cost)}.${dedupNote}`,
    estimatedSavings: {
      tokensPerSession: ghost.sizeTokens,
      usdPerSession: perSessionUsd,
    },
    actions: [action],
  };
}

export function ghostFindingsToWasteFindings(
  ghosts: GhostSurfaceFinding[],
  options: GhostSurfaceFindingOptions = {},
): WasteFinding[] {
  return ghosts.map((g) => ghostSurfaceToFinding(g, options));
}
