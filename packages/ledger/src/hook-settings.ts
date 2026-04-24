import { randomUUID } from 'node:crypto';

export interface BuildClaudeHookSettingsOptions {
  // Path (or bare name) of the burn binary. Defaults to `burn`, which Claude
  // Code's shell will resolve against PATH at hook-fire time.
  burnBin?: string;
}

export interface ClaudeHookSettingsResult {
  sessionId: string;
  // JSON string ready to be passed to `claude --settings`. Inline JSON is
  // supported by Claude Code in addition to file paths.
  settings: string;
}

// Event names Claude Code will dispatch hook payloads for. Each fires with a
// JSON payload on stdin that includes `session_id` and `transcript_path`;
// `burn ingest --runtime claude` consumes that uniformly and incrementally
// parses the transcript. Safe to register all of them — the ledger's cursor +
// dedup index make repeated invocations idempotent.
//
// Tool-call failures do NOT get a distinct hook event in Claude Code — the
// regular `PostToolUse` payload already carries the failure signal via the
// tool_result block's `is_error` flag, which the reader surfaces on each
// `ToolCall.isError`. Don't add a phantom `PostToolUseFailure` here: Claude
// Code treats unknown hook event names as a settings error.
const TOOL_MATCHED_EVENTS = ['PreToolUse', 'PostToolUse'] as const;
const UNMATCHED_EVENTS = [
  'UserPromptSubmit',
  'Notification',
  'Stop',
  'SubagentStop',
  'SessionEnd',
] as const;

type HookEvent =
  | (typeof TOOL_MATCHED_EVENTS)[number]
  | (typeof UNMATCHED_EVENTS)[number];

interface HookCommand {
  type: 'command';
  command: string;
}

interface HookMatcher {
  matcher?: string;
  hooks: HookCommand[];
}

export function buildClaudeHookSettings(
  options: BuildClaudeHookSettingsOptions = {},
): ClaudeHookSettingsResult {
  const sessionId = randomUUID();
  const burnBin = options.burnBin ?? 'burn';
  const command = `${shellQuote(burnBin)} ingest --runtime claude --quiet`;

  const hooks: Partial<Record<HookEvent, HookMatcher[]>> = {};
  for (const evt of TOOL_MATCHED_EVENTS) {
    hooks[evt] = [{ matcher: '*', hooks: [{ type: 'command', command }] }];
  }
  for (const evt of UNMATCHED_EVENTS) {
    hooks[evt] = [{ hooks: [{ type: 'command', command }] }];
  }

  return { sessionId, settings: JSON.stringify({ hooks }) };
}

// Minimal shell-safe quoting for the burnBin argument. Claude Code runs the
// hook command through a shell, so a path with spaces or quotes would
// otherwise break the exec. Absolute binary paths almost never contain
// metacharacters, but the cost of getting this wrong is a silently-broken
// hook, so wrap defensively whenever the value isn't a plain identifier.
function shellQuote(value: string): string {
  if (/^[A-Za-z0-9_./-]+$/.test(value)) return value;
  return `'${value.replace(/'/g, `'\\''`)}'`;
}
