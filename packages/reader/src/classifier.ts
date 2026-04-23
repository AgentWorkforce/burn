import type { ActivityCategory, ToolCall } from './types.js';

export interface ClassificationInput {
  toolCalls: ToolCall[];
  // Message text used for keyword refinement. Typically the preceding user
  // prompt, optionally concatenated with the assistant's own text blocks.
  text?: string;
  // True if any tool_result block for this turn reported is_error.
  hasFailedTool?: boolean;
  // Reasoning tokens billed on this turn. When > 0 and no tools / no keyword
  // refinement fires, the turn is classified as 'reasoning' instead of
  // 'conversation' — big reasoning-only turns are expensive and worth
  // distinguishing from chit-chat.
  reasoningTokens?: number;
}

export interface ClassificationResult {
  activity: ActivityCategory;
  retries: number;
  hasEdits: boolean;
}

const EDIT_TOOLS = new Set(['Edit', 'Write', 'NotebookEdit', 'MultiEdit']);
const DELEGATION_TOOLS = new Set(['Agent', 'Task']);
const READ_ONLY_TOOLS = new Set([
  'Read',
  'Grep',
  'Glob',
  'WebFetch',
  'WebSearch',
  'LS',
]);

// Map harness-specific tool names to the canonical (Claude Code) names the
// rule tables above are written against. The classifier is rule-based and
// deterministic, so adding a new harness is just adding its tool names here.
const TOOL_ALIASES: Record<string, string> = {
  // Codex
  apply_patch: 'Edit',
  exec_command: 'Bash',
  shell: 'Bash',
  read_file: 'Read',
  write_file: 'Write',
  update_plan: 'ExitPlanMode',
  spawn_agent: 'Agent',
  send_input: 'Task',
  wait_agent: 'Task',
  close_agent: 'Task',
  resume_agent: 'Task',
  view_image: 'Read',
  read_mcp_resource: 'Read',
  // OpenCode (lowercase names)
  read: 'Read',
  write: 'Write',
  edit: 'Edit',
  bash: 'Bash',
  grep: 'Grep',
  glob: 'Glob',
  webfetch: 'WebFetch',
  task: 'Task',
};

export function normalizeToolName(name: string): string {
  return TOOL_ALIASES[name] ?? name;
}

// Bash command heuristics. Match on the first non-env token after stripping
// leading environment assignments (e.g. "CI=1 pytest" → "pytest").
const TEST_PATTERNS: RegExp[] = [
  /\bpytest\b/,
  /\bpython\s+-m\s+pytest\b/,
  /\bvitest\b/,
  /\bbun\s+test\b/,
  /\bjest\b/,
  /\bmocha\b/,
  /\brspec\b/,
  /\bphpunit\b/,
  /\bgo\s+test\b/,
  /\bcargo\s+test\b/,
  /\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?test\b/,
  /\bnode\s+--test\b/,
  /\bmake\s+test\b/,
  /\bctest\b/,
  // e2e / browser runners
  /\bplaywright\b/,
  /\bcypress\b/,
  /\bpuppeteer\b/,
];

const REVIEW_PATTERNS: RegExp[] = [
  /\bgit\s+status\b/,
  /\bgit\s+diff\b/,
  /\bgit\s+show\b/,
  /\bgit\s+log\b/,
  /\bgit\s+blame\b/,
  /\bgh\s+pr\s+(?:view|diff|checks)\b/,
  /\bgh\s+run\s+view\b/,
];

const GIT_PATTERNS: RegExp[] = [
  /\bgit\s+(?:push|pull|fetch|commit|merge|rebase|checkout|cherry-pick|reset|revert|switch|tag|stash)\b/,
];

// Dependency-management commands live between "build" and "exploration" on the
// priority ladder — they're purposeful actions (not incidental) but they're
// also the clearest waste-pattern signal we can detect from bash alone.
const DEPS_PATTERNS: RegExp[] = [
  /\b(?:npm|yarn|pnpm|bun)\s+(?:install|add|remove|uninstall|update|upgrade|ci)\b/,
  /\bpip\s+(?:install|uninstall)\b/,
  /\bpip3\s+(?:install|uninstall)\b/,
  /\bpython\s+-m\s+pip\s+(?:install|uninstall)\b/,
  /\buv\s+(?:add|remove|sync|pip\s+install)\b/,
  /\bpoetry\s+(?:add|remove|install|update)\b/,
  /\bcargo\s+(?:add|remove|update)\b/,
  /\bgo\s+(?:get|mod\s+(?:tidy|download))\b/,
  /\bbundle\s+(?:install|update|add)\b/,
  /\bgem\s+(?:install|uninstall)\b/,
  /\bbrew\s+(?:install|uninstall|upgrade|update)\b/,
  /\bapt(?:-get)?\s+(?:install|remove)\b/,
];

const FORMAT_PATTERNS: RegExp[] = [
  /\bprettier\b.*(?:--write|-w)(?:\s|$)/,
  /\beslint\b.*--fix\b/,
  /\bbiome\s+format\b/,
  /\bbiome\s+check\b.*--apply\b/,
  /\bblack\b(?!.*--check\b)/,
  /\bruff\s+format\b/,
  /\bisort\b/,
  /\brustfmt\b/,
  /\bcargo\s+fmt\b(?!.*--check\b)/,
  /\bgofmt\b/,
  /\bgoimports\b/,
  /\bdprint\s+fmt\b/,
];

const VERIFICATION_PATTERNS: RegExp[] = [
  /\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?lint\b/,
  /\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?typecheck\b/,
  /\bprettier\b.*--check\b/,
  /\beslint\b(?!.*--fix\b)/,
  /\bbiome\s+check\b(?!.*--apply\b)/,
  /\bblack\b.*--check\b/,
  /\bruff\s+check\b/,
  /\bflake8\b/,
  /\bmypy\b/,
  /\bpyright\b/,
  /\btsc\b(?!\s+--build\b)/,
  /\bcargo\s+check\b/,
  /\bcargo\s+fmt\b.*--check\b/,
  /\bgolangci-lint\b/,
  /\bshellcheck\b/,
  /\bhadolint\b/,
  /\bterraform\s+validate\b/,
  /\bmake\s+(?:lint|check|typecheck|verify)\b/,
];

const BUILD_DEPLOY_PATTERNS: RegExp[] = [
  /\bdocker\s+(?:build|compose\s+build|push)\b/,
  /\bcargo\s+build\b/,
  /\bgo\s+build\b/,
  /\bmake\s+(?:build|release|dist|package|deploy)\b/,
  /\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?build\b/,
  /\b(?:webpack|vite|next|rollup|esbuild)\s+build\b/,
  /\btsc\s+--build\b/,
  /\bpm2\s+/,
  /\bkubectl\s+(?:apply|rollout|set)\b/,
  /\bhelm\s+(?:install|upgrade)\b/,
  /\bterraform\s+(?:apply|plan)\b/,
  /\bterraform\s+destroy\b/,
  /\bserverless\s+deploy\b/,
  /\b(?:vercel|netlify|flyctl|railway|sst)\s+(?:deploy|up)\b/,
];

// Files that count as "documentation" when edited. Matched against the
// ToolCall.target on edit tools. Also recognizes docs/** and README* paths.
const DOC_FILE_PATTERNS: RegExp[] = [
  /\.md$/i,
  /\.mdx$/i,
  /\.rst$/i,
  /\.adoc$/i,
  /\.txt$/i,
  /(?:^|\/)README(?:\.[^/]*)?$/i,
  /(?:^|\/)CHANGELOG(?:\.[^/]*)?$/i,
  /(?:^|\/)docs\//,
];

const DEBUG_RE =
  /\b(bug|error|crash|traceback|stack\s*trace|failure|failing|broken|fix\s+the|not\s+working|throws?)\b/i;
const REVIEW_RE = /\b(review|audit|inspect|look\s+over|code\s+review|pr\s+review)\b/i;
const REFACTOR_RE =
  /\b(refactor|refactoring|cleanup|clean\s+up|rename|extract|restructure|move\s+this|reorganize)\b/i;
const FEATURE_RE =
  /\b(add|create|implement|new\s+feature|build\s+the|introduce|support\s+for)\b/i;
const BRAINSTORM_RE =
  /\b(brainstorm|what\s+if|think\s+through|explore(?:\s+ideas)?|design|should\s+we|approach(?:es)?)\b/i;
const PLANNING_RE = /\b(plan(?:ning)?|outline|roadmap|strategy)\b/i;

export function classifyActivity(input: ClassificationInput): ClassificationResult {
  const toolCalls = input.toolCalls ?? [];
  const text = input.text ?? '';
  const hasFailedTool = input.hasFailedTool === true;
  const reasoningTokens = input.reasoningTokens ?? 0;
  const hasEdits = toolCalls.some((t) => EDIT_TOOLS.has(normalizeToolName(t.name)));
  const retries = countRetries(toolCalls);

  const activity = pickCategory({
    toolCalls,
    text,
    hasFailedTool,
    hasEdits,
    retries,
    reasoningTokens,
  });
  return { activity, retries, hasEdits };
}

interface PickInput {
  toolCalls: ToolCall[];
  text: string;
  hasFailedTool: boolean;
  hasEdits: boolean;
  retries: number;
  reasoningTokens: number;
}

function pickCategory({
  toolCalls,
  text,
  hasFailedTool,
  hasEdits,
  retries,
  reasoningTokens,
}: PickInput): ActivityCategory {
  // Priority 1: delegation — spawning a subagent dominates whatever else happened.
  if (toolCalls.some((t) => DELEGATION_TOOLS.has(normalizeToolName(t.name)))) return 'delegation';

  // Priority 2: explicit plan-mode marker.
  if (toolCalls.some((t) => normalizeToolName(t.name) === 'ExitPlanMode')) return 'planning';

  // Priority 3: edits present. Let keyword refinement pick a sub-category;
  // fall through to documentation / debugging / coding as appropriate.
  if (hasEdits) {
    if (hasFailedTool) return 'debugging';
    // >= 2 retries (edit→bash→edit→bash→edit) inside a single turn is almost
    // always the model chasing a bug. Call it debugging even without an
    // explicit error signal.
    if (retries >= 2) return 'debugging';
    if (allEditsAreDocs(toolCalls)) return 'docs';
    const refined = refineEditByKeywords(text);
    if (refined) return refined;
    return 'coding';
  }

  // Priority 4: a failed tool call on a non-edit turn is debugging — a failing
  // pytest / git push / build command is the model reacting to an error, not
  // neutrally running tests or pushing code.
  if (hasFailedTool) return 'debugging';

  // Priority 5: bash commands with recognizable patterns. Test/git match
  // first so that 'npm test' doesn't collide with the generic npm deps rule.
  const bashCalls = toolCalls.filter((t) => normalizeToolName(t.name) === 'Bash');
  for (const call of bashCalls) {
    const cmd = stripEnv(call.target ?? '');
    if (!cmd) continue;
    if (TEST_PATTERNS.some((re) => re.test(cmd))) return 'testing';
    if (REVIEW_PATTERNS.some((re) => re.test(cmd))) return 'review';
    if (GIT_PATTERNS.some((re) => re.test(cmd))) return 'git';
    if (DEPS_PATTERNS.some((re) => re.test(cmd))) return 'deps';
    if (FORMAT_PATTERNS.some((re) => re.test(cmd))) return 'format';
    if (VERIFICATION_PATTERNS.some((re) => re.test(cmd))) return 'verification';
    if (BUILD_DEPLOY_PATTERNS.some((re) => re.test(cmd))) return 'build-deploy';
  }

  // Priority 6: any tools used at all → exploration (read-only, MCP, skills,
  // or un-patterned bash). Keyword hints can still promote the category.
  if (toolCalls.length > 0) {
    const refined = refineIntentByKeywords(text);
    if (refined) return refined;
    if (toolCalls.some((t) => READ_ONLY_TOOLS.has(normalizeToolName(t.name)))) return 'exploration';
    return 'exploration';
  }

  // Priority 7: no tools — keyword-only classification.
  const refined = hasFailedTool ? 'debugging' : refineIntentByKeywords(text);
  if (refined) return refined;
  if (BRAINSTORM_RE.test(text)) return 'brainstorming';
  if (PLANNING_RE.test(text)) return 'planning';
  // Reasoning-only turns (extended thinking, Codex reasoning tokens) with no
  // tools and no user-text hook are distinct from chit-chat — they carry a
  // very different cost profile.
  if (reasoningTokens > 0) return 'reasoning';
  return 'conversation';
}

function allEditsAreDocs(toolCalls: ToolCall[]): boolean {
  const edits = toolCalls.filter((t) => EDIT_TOOLS.has(normalizeToolName(t.name)));
  if (edits.length === 0) return false;
  return edits.every((t) => {
    const target = t.target;
    if (typeof target !== 'string' || target.length === 0) return false;
    return DOC_FILE_PATTERNS.some((re) => re.test(target));
  });
}

function refineEditByKeywords(text: string): ActivityCategory | null {
  if (!text) return null;
  if (DEBUG_RE.test(text)) return 'debugging';
  if (REFACTOR_RE.test(text)) return 'refactoring';
  if (FEATURE_RE.test(text)) return 'feature';
  return null;
}

function refineIntentByKeywords(text: string): ActivityCategory | null {
  if (!text) return null;
  if (DEBUG_RE.test(text)) return 'debugging';
  if (REVIEW_RE.test(text)) return 'review';
  if (REFACTOR_RE.test(text)) return 'refactoring';
  if (FEATURE_RE.test(text)) return 'feature';
  return null;
}

function stripEnv(cmd: string): string {
  // Strip leading `FOO=bar BAZ=qux` assignments so `CI=1 pytest` matches pytest.
  return cmd.replace(/^(?:\s*[A-Z_][A-Z0-9_]*=\S+\s+)+/, '');
}

// Count edit→bash→edit cycles within a single turn. A "cycle" ends when an
// edit tool appears after at least one bash call that followed an earlier edit.
// Example: [Edit, Bash, Edit] → 1. [Edit, Bash, Edit, Bash, Edit] → 2.
// [Edit, Edit] → 0 (no bash in between). [Bash, Edit, Edit] → 0.
export function countRetries(toolCalls: ToolCall[]): number {
  let retries = 0;
  let seenEdit = false;
  let seenBashAfterEdit = false;
  for (const tc of toolCalls) {
    const name = normalizeToolName(tc.name);
    if (EDIT_TOOLS.has(name)) {
      if (seenEdit && seenBashAfterEdit) {
        retries++;
        seenBashAfterEdit = false;
      }
      seenEdit = true;
    } else if (name === 'Bash' && seenEdit) {
      seenBashAfterEdit = true;
    }
  }
  return retries;
}
