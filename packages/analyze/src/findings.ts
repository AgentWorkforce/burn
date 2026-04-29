// Structured envelope for waste-detector output. Closes #56.
//
// Each per-detector struct in `patterns.ts` (`RetryLoop`, `FailureRun`,
// `CompactionLoss`, `EditRevertCycle`, `EditHeavySession`, `SkillRecallDup`,
// `SkillPruningProtection`, `SystemPromptTax`) keeps its narrow shape for
// downstream consumers that want it. This module wraps each one in a common
// `WasteFinding` shape so the CLI can render every detector through one
// table renderer, severity-rank a heterogeneous list, and (eventually) drive
// a confirmation-gated `burn hotspots --apply` pipeline against typed
// `WasteAction`s instead of scraping strings.

import type {
  CancellationRun,
  CompactionLoss,
  EditHeavySession,
  EditRevertCycle,
  FailureRun,
  PatternEventSource,
  PatternsResult,
  RetryLoop,
  SkillPruningProtection,
  SkillRecallDup,
  SystemPromptTax,
} from './patterns.js';

// A typed action a finding suggests the user (or `burn hotspots --apply`) take.
// `paste` is text the user copies somewhere (CLAUDE.md, an agent prompt, a
// chat message); `command` is a shell command; `file-content` is a full file
// body to write to disk. Keeping the union closed lets `--apply` decide what
// is safe to execute automatically vs. what needs explicit user action.
export type WasteAction =
  | { type: 'paste'; label: string; text: string }
  | { type: 'command'; label: string; text: string }
  | { type: 'file-content'; label: string; path: string; content: string };

export type WasteSeverity = 'info' | 'warn' | 'high';

export interface EstimatedSavings {
  // Tokens saved per session if this finding is addressed. Omitted when the
  // detector can't estimate it without content-sidecar sizes.
  tokensPerSession?: number;
  // Dollar savings per session, in USD. Mirrors the underlying detector's
  // cost field.
  usdPerSession?: number;
  // Projected monthly savings. Detectors don't compute this (would need a
  // session-frequency model); reserved for future aggregators that fold
  // findings across many sessions.
  usdPerMonth?: number;
}

export interface WasteFinding {
  // Stable kind tag. Matches the `--patterns` flag value where applicable
  // (`retry-loop`, `failure-run`, `cancellation-run`, `compaction-loss`,
  // `edit-revert`, `edit-heavy`, `skill-recall-dup`, `skill-pruning-protection`,
  // `system-prompt-tax`). New detectors should pick a kebab-case noun.
  kind: string;
  severity: WasteSeverity;
  sessionId: string;
  title: string;
  detail: string;
  estimatedSavings: EstimatedSavings;
  actions: WasteAction[];
  // Present for graph-backed retry/failure/cancellation findings.
  eventSource?: PatternEventSource;
}

// Severity tiers driven by `usdPerSession`. Consistent across detectors so
// rankings on a heterogeneous list are comparable. Detectors with no cost
// signal (e.g. compaction events with zero `cacheLostCost`) fall into `info`.
const SEVERITY_HIGH_USD = 0.5;
const SEVERITY_WARN_USD = 0.05;

function severityFromUsd(usd: number): WasteSeverity {
  if (usd >= SEVERITY_HIGH_USD) return 'high';
  if (usd >= SEVERITY_WARN_USD) return 'warn';
  return 'info';
}

function hotspotsAction(sessionId: string): WasteAction {
  return {
    type: 'command',
    label: 'Inspect this session',
    text: `burn hotspots --session ${sessionId}`,
  };
}

function fmtUsd(n: number): string {
  return `$${n.toFixed(4)}`;
}

export function retryLoopToFinding(loop: RetryLoop): WasteFinding {
  const target = loop.target ? ` ${loop.target}` : '';
  // Enrichment (#57): if the content sidecar surfaced an error signature,
  // append it to the title so the report is actionable at a glance ("4×
  // Bash retries" → "4× Bash retries: 'npm ERR! code ENOENT'").
  const titleSuffix = loop.errorSignature ? `: '${loop.errorSignature}'` : '';
  const finding: WasteFinding = {
    kind: 'retry-loop',
    severity: severityFromUsd(loop.cost),
    sessionId: loop.sessionId,
    title: `Retry loop: ${loop.tool}${target} failed ${loop.attempts}× in a row${titleSuffix}`,
    detail:
      `Turns ${loop.startTurnIndex}-${loop.endTurnIndex} are ${loop.attempts} consecutive ` +
      `errored ${loop.tool} calls with the same arguments. Cumulative turn cost ` +
      `${fmtUsd(loop.cost)} — the agent kept retrying without changing inputs.`,
    estimatedSavings: { usdPerSession: loop.cost },
    actions: [hotspotsAction(loop.sessionId)],
  };
  if (loop.eventSource !== undefined) finding.eventSource = loop.eventSource;
  return finding;
}

export function failureRunToFinding(run: FailureRun): WasteFinding {
  // Enrichment (#57): when content surfaced per-tool error signatures, fold
  // them into the detail text so the user can diagnose the stuck state
  // without opening the session file.
  const sigDetail =
    run.errorSignatures && run.errorSignatures.length > 0
      ? ' Errors: ' +
        run.errorSignatures.map((s) => `${s.tool}='${s.firstLine}'`).join('; ') +
        '.'
      : '';
  const finding: WasteFinding = {
    kind: 'failure-run',
    severity: severityFromUsd(run.cost),
    sessionId: run.sessionId,
    title: `Failure run: ${run.length} consecutive failed tool calls`,
    detail:
      `Turns ${run.startTurnIndex}-${run.endTurnIndex} failed across ` +
      `${run.toolsInvolved.length} distinct tool(s) (${run.toolsInvolved.join(', ')}). ` +
      `Cumulative turn cost ${fmtUsd(run.cost)} — agent likely stuck without ` +
      `recovering or asking for help.${sigDetail}`,
    estimatedSavings: { usdPerSession: run.cost },
    actions: [hotspotsAction(run.sessionId)],
  };
  if (run.eventSource !== undefined) finding.eventSource = run.eventSource;
  return finding;
}

export function cancellationRunToFinding(run: CancellationRun): WasteFinding {
  const toolList = run.toolsInvolved.join(', ');
  return {
    kind: 'cancellation-run',
    severity: severityFromUsd(run.cost),
    sessionId: run.sessionId,
    title: `Cancellation run: ${run.length} cancelled tool call${run.length === 1 ? '' : 's'}`,
    detail:
      `Turns ${run.startTurnIndex}-${run.endTurnIndex} ended with cancelled ` +
      `tool/subagent status (${toolList}). Cumulative turn cost ${fmtUsd(run.cost)}.`,
    estimatedSavings: { usdPerSession: run.cost },
    actions: [hotspotsAction(run.sessionId)],
    eventSource: run.eventSource,
  };
}

export function compactionLossToFinding(loss: CompactionLoss): WasteFinding {
  const savings: EstimatedSavings = { usdPerSession: loss.cacheLostCost };
  if (loss.tokensBeforeCompact > 0) savings.tokensPerSession = loss.tokensBeforeCompact;
  // Enrichment (#57): describe what was compacted instead of the bare token
  // count when the content sidecar is available.
  const work = loss.lostWork;
  const lostWorkDetail = work
    ? ` Compacted window: ${work.editCount} edit(s), ${work.bashCount} bash, ` +
      `${work.readCount} read(s)` +
      (work.files.length > 0
        ? ' on ' +
          (work.files.length <= 3
            ? work.files.join(', ')
            : `${work.files.slice(0, 3).join(', ')} +${work.files.length - 3} more`)
        : '') +
      '.'
    : '';
  return {
    kind: 'compaction-loss',
    severity: severityFromUsd(loss.cacheLostCost),
    sessionId: loss.sessionId,
    title: `Compaction lost ${loss.tokensBeforeCompact.toLocaleString()} cached tokens`,
    detail:
      `A compaction at ${loss.ts} discarded ${loss.tokensBeforeCompact.toLocaleString()} ` +
      `tokens of cache. Pre-compact cacheRead cost ${fmtUsd(loss.cacheLostCost)} — that ` +
      `cache won't be reused on subsequent turns.${lostWorkDetail}`,
    estimatedSavings: savings,
    actions: [hotspotsAction(loss.sessionId)],
  };
}

export function editRevertToFinding(cycle: EditRevertCycle): WasteFinding {
  // Enrichment (#57): show the actual strings that were thrashed so users
  // don't need to grep the session file. Truncated by the detector to
  // ~200 chars per field.
  const preview = cycle.samplePreview;
  const previewDetail = preview
    ? ` First edit: '${preview.firstEdit.old}' → '${preview.firstEdit.new}'. ` +
      `Revert: '${preview.revert.old}' → '${preview.revert.new}'.`
    : '';
  return {
    kind: 'edit-revert',
    severity: severityFromUsd(cycle.cost),
    sessionId: cycle.sessionId,
    title: `Edit revert on ${cycle.filePath}`,
    detail:
      `Turn ${cycle.firstEditTurnIndex} edited ${cycle.filePath}; turn ${cycle.revertTurnIndex} ` +
      `restored a prior file state ${cycle.spanTurns} turns later. Cumulative anchor-turn ` +
      `cost ${fmtUsd(cycle.cost)} — the intermediate work was erased.${previewDetail}`,
    estimatedSavings: { usdPerSession: cycle.cost },
    actions: [hotspotsAction(cycle.sessionId)],
  };
}

export function editHeavyToFinding(session: EditHeavySession): WasteFinding {
  const ratioStr = Number.isFinite(session.ratio) ? session.ratio.toFixed(1) : '∞';
  return {
    kind: 'edit-heavy',
    // Edit-heavy doesn't add to totalPatternCost (overlaps with retry/revert
    // costs), so we score severity off the underlying cost but cap it: this
    // is a "fuzzy" signal, not a per-event waste loss.
    severity: severityFromUsd(session.cost) === 'high' ? 'warn' : severityFromUsd(session.cost),
    sessionId: session.sessionId,
    title: `Edit-heavy session: ${session.editCount} edits / ${session.readCount} reads (ratio ${ratioStr})`,
    detail:
      `${session.source} session has ${session.editCount} edit-tool calls against only ` +
      `${session.readCount} read-tool calls (ratio ${ratioStr}, threshold 4×). ` +
      `${session.likelyRetries} edit→bash→edit retry pattern(s) observed. Edit-bearing ` +
      `turn cost ${fmtUsd(session.cost)} — careless editing without first reading ` +
      `surrounding context.`,
    estimatedSavings: { usdPerSession: session.cost },
    actions: [hotspotsAction(session.sessionId)],
  };
}

export function skillRecallDupToFinding(dup: SkillRecallDup): WasteFinding {
  return {
    kind: 'skill-recall-dup',
    severity: severityFromUsd(dup.cost),
    sessionId: dup.sessionId,
    title: `OpenCode skill "${dup.skillName}" called ${dup.callCount}× without dedup`,
    detail:
      `OpenCode does not deduplicate skill tool results, so each of the ${dup.callCount} ` +
      `calls (turns ${dup.firstTurnIndex}-${dup.lastTurnIndex}) re-injects the full ` +
      `SKILL.md content into context. Cumulative turn cost ${fmtUsd(dup.cost)}.`,
    estimatedSavings: { usdPerSession: dup.cost },
    actions: [hotspotsAction(dup.sessionId)],
  };
}

export function skillPruningProtectionToFinding(
  prot: SkillPruningProtection,
): WasteFinding {
  return {
    kind: 'skill-pruning-protection',
    severity: severityFromUsd(prot.cost),
    sessionId: prot.sessionId,
    title: `OpenCode skill "${prot.skillName}" rode in cache ${prot.ridingTurns} turn(s)`,
    detail:
      `Skill tool results are listed in OpenCode's PRUNE_PROTECTED_TOOLS and never ` +
      `evict during compaction. Invoked at turn ${prot.invokedTurnIndex}; still in ` +
      `cacheRead at turn ${prot.lastCachedTurnIndex}. Invoke + riding-turn cost ` +
      `${fmtUsd(prot.cost)}.`,
    estimatedSavings: { usdPerSession: prot.cost },
    actions: [hotspotsAction(prot.sessionId)],
  };
}

export function systemPromptTaxToFinding(tax: SystemPromptTax): WasteFinding {
  // Prefix tokens that ride in cache on every turn after the first.
  const ridingTokens = tax.estimatedSystemPromptTokens * tax.ridingTurns;
  return {
    kind: 'system-prompt-tax',
    severity: severityFromUsd(tax.totalCost),
    sessionId: tax.sessionId,
    title: `OpenCode system prompt tax: ~${tax.estimatedSystemPromptTokens.toLocaleString()} tokens × ${tax.ridingTurns} turn(s)`,
    detail:
      `First-turn cacheCreate of ${tax.firstTurnCacheCreate.toLocaleString()} tokens minus ` +
      `the first user message (${tax.firstUserMessageTokens.toLocaleString()}) leaves ` +
      `~${tax.estimatedSystemPromptTokens.toLocaleString()} tokens of system prompt + ` +
      `skill catalog riding cacheRead across ${tax.ridingTurns} subsequent turn(s). ` +
      `Total cost ${fmtUsd(tax.totalCost)}.`,
    estimatedSavings: {
      tokensPerSession: ridingTokens,
      usdPerSession: tax.totalCost,
    },
    actions: [hotspotsAction(tax.sessionId)],
  };
}

// Roll the full PatternsResult into a single severity-ranked list. Within
// the same severity tier, sort by `usdPerSession` descending so the most
// expensive findings surface first.
export function findingsFromPatterns(result: PatternsResult): WasteFinding[] {
  const findings: WasteFinding[] = [];
  for (const r of result.retryLoops) findings.push(retryLoopToFinding(r));
  for (const f of result.failureRuns) findings.push(failureRunToFinding(f));
  for (const c of result.cancelledRuns) findings.push(cancellationRunToFinding(c));
  for (const c of result.compactions) findings.push(compactionLossToFinding(c));
  for (const e of result.editReverts) findings.push(editRevertToFinding(e));
  for (const e of result.editHeavySessions) findings.push(editHeavyToFinding(e));
  for (const d of result.skillRecallDups) findings.push(skillRecallDupToFinding(d));
  for (const p of result.skillPruningProtection) findings.push(skillPruningProtectionToFinding(p));
  for (const s of result.systemPromptTaxes) findings.push(systemPromptTaxToFinding(s));
  return sortFindings(findings);
}

const SEVERITY_ORDER: Record<WasteSeverity, number> = { high: 0, warn: 1, info: 2 };

export function sortFindings(findings: WasteFinding[]): WasteFinding[] {
  return [...findings].sort((a, b) => {
    const sevDiff = SEVERITY_ORDER[a.severity] - SEVERITY_ORDER[b.severity];
    if (sevDiff !== 0) return sevDiff;
    const aUsd = a.estimatedSavings.usdPerSession ?? 0;
    const bUsd = b.estimatedSavings.usdPerSession ?? 0;
    return bUsd - aUsd;
  });
}
