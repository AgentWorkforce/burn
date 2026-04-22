import type { ActivityCategory, ToolCall } from './types.js';

export interface ClassificationInput {
  toolCalls: ToolCall[];
  // Message text used for keyword refinement. Typically the preceding user
  // prompt, optionally concatenated with the assistant's own text blocks.
  text?: string;
  // True if any tool_result block for this turn reported is_error.
  hasFailedTool?: boolean;
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
];

const GIT_PATTERNS: RegExp[] = [
  /\bgit\s+(?:push|pull|fetch|commit|merge|rebase|checkout|cherry-pick|reset|revert|switch|tag|stash)\b/,
];

const BUILD_DEPLOY_PATTERNS: RegExp[] = [
  /\bdocker\s+(?:build|compose\s+build|push)\b/,
  /\bcargo\s+build\b/,
  /\bgo\s+build\b/,
  /\bmake(?:\s|$)/,
  /\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?build\b/,
  /\b(?:webpack|vite|next|rollup|esbuild)\s+build\b/,
  /\btsc\s+--build\b/,
  /\bpm2\s+/,
  /\bkubectl\s+(?:apply|rollout|set)\b/,
  /\bterraform\s+(?:apply|plan)\b/,
  /\bserverless\s+deploy\b/,
  /\bdeploy\b/,
];

const DEBUG_RE =
  /\b(bug|error|crash|traceback|stack\s*trace|failure|failing|broken|fix\s+the|not\s+working|throws?)\b/i;
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
  const hasEdits = toolCalls.some((t) => EDIT_TOOLS.has(t.name));
  const retries = countRetries(toolCalls);

  const activity = pickCategory({ toolCalls, text, hasFailedTool, hasEdits });
  return { activity, retries, hasEdits };
}

interface PickInput {
  toolCalls: ToolCall[];
  text: string;
  hasFailedTool: boolean;
  hasEdits: boolean;
}

function pickCategory({ toolCalls, text, hasFailedTool, hasEdits }: PickInput): ActivityCategory {
  // Priority 1: explicit plan-mode marker.
  if (toolCalls.some((t) => t.name === 'ExitPlanMode')) return 'planning';

  // Priority 2: delegation — spawning a subagent dominates whatever else happened.
  if (toolCalls.some((t) => DELEGATION_TOOLS.has(t.name))) return 'delegation';

  // Priority 3: edits present. Let keyword refinement pick a sub-category;
  // default to coding if nothing stronger fires.
  if (hasEdits) {
    const refined = refineByKeywords(text, hasFailedTool);
    return refined ?? 'coding';
  }

  // Priority 4: bash commands with recognizable patterns.
  const bashCalls = toolCalls.filter((t) => t.name === 'Bash');
  for (const call of bashCalls) {
    const cmd = stripEnv(call.target ?? '');
    if (!cmd) continue;
    if (TEST_PATTERNS.some((re) => re.test(cmd))) return 'testing';
    if (GIT_PATTERNS.some((re) => re.test(cmd))) return 'git';
    if (BUILD_DEPLOY_PATTERNS.some((re) => re.test(cmd))) return 'build-deploy';
  }

  // Priority 5: any tools used at all → exploration (read-only, MCP, skills,
  // or un-patterned bash). Keyword hints can still promote to debugging etc.
  if (toolCalls.length > 0) {
    if (hasFailedTool) return 'debugging';
    const refined = refineByKeywords(text, false);
    if (refined) return refined;
    if (toolCalls.some((t) => READ_ONLY_TOOLS.has(t.name))) return 'exploration';
    return 'exploration';
  }

  // Priority 6: no tools — keyword-only classification.
  const refined = refineByKeywords(text, hasFailedTool);
  if (refined) return refined;
  if (BRAINSTORM_RE.test(text)) return 'brainstorming';
  if (PLANNING_RE.test(text)) return 'planning';
  return 'conversation';
}

function refineByKeywords(text: string, hasFailedTool: boolean): ActivityCategory | null {
  if (hasFailedTool) return 'debugging';
  if (!text) return null;
  if (DEBUG_RE.test(text)) return 'debugging';
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
    if (EDIT_TOOLS.has(tc.name)) {
      if (seenEdit && seenBashAfterEdit) {
        retries++;
        seenBashAfterEdit = false;
      }
      seenEdit = true;
    } else if (tc.name === 'Bash' && seenEdit) {
      seenBashAfterEdit = true;
    }
  }
  return retries;
}
