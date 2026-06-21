//! Structured envelope for waste-detector output. Rust port of
//! `packages/analyze/src/findings.ts`.
//!
//! Each per-detector struct (`RetryLoop`, `FailureRun`, `CompactionLoss`,
//! `EditRevertCycle`, `EditHeavySession`, `SkillRecallDup`,
//! `SkillPruningProtection`, `SystemPromptTax`) keeps its narrow shape for
//! downstream consumers that want it. This module wraps each one in a common
//! `WasteFinding` shape so the CLI can render every detector through one
//! table renderer, severity-rank a heterogeneous list, and (eventually) drive
//! a confirmation-gated `burn hotspots --apply` pipeline against typed
//! `WasteAction`s instead of scraping strings.
//!
//! The pattern struct types referenced by the `*ToFinding` adapters live in
//! this module so the future `patterns.rs` port can depend on it without a
//! circular dependency. See AgentWorkforce/burn#268.

use crate::reader::{SourceKind, ToolResultEventSource};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Pattern struct types — concrete shapes consumed by the adapters below.
// These mirror the TS interfaces in `packages/analyze/src/patterns.ts`. The
// upcoming `patterns.rs` port re-exports / re-uses them so detector code and
// finding adapters share one set of shapes.
// ---------------------------------------------------------------------------

/// Either a real `ToolResultEventSource` (passed through verbatim) or
/// `Mixed`, used when a finding spans event records of differing sources.
/// Serializes to the same kebab/snake-case strings as the TS union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternEventSource {
    ToolResult,
    SubagentNotification,
    QueueEvent,
    ProgressEvent,
    FunctionCallOutput,
    Mixed,
}

impl From<ToolResultEventSource> for PatternEventSource {
    fn from(src: ToolResultEventSource) -> Self {
        match src {
            ToolResultEventSource::ToolResult => PatternEventSource::ToolResult,
            ToolResultEventSource::SubagentNotification => PatternEventSource::SubagentNotification,
            ToolResultEventSource::QueueEvent => PatternEventSource::QueueEvent,
            ToolResultEventSource::ProgressEvent => PatternEventSource::ProgressEvent,
            ToolResultEventSource::FunctionCallOutput => PatternEventSource::FunctionCallOutput,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryLoop {
    pub session_id: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub args_hash: String,
    pub attempts: u64,
    pub start_turn_index: u64,
    pub end_turn_index: u64,
    pub cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_source: Option<PatternEventSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureRunErrorSignature {
    pub tool: String,
    pub first_line: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureRun {
    pub session_id: String,
    pub length: u64,
    pub start_turn_index: u64,
    pub end_turn_index: u64,
    pub tools_involved: Vec<String>,
    pub cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_signatures: Option<Vec<FailureRunErrorSignature>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_source: Option<PatternEventSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancellationRun {
    pub session_id: String,
    pub length: u64,
    pub start_turn_index: u64,
    pub end_turn_index: u64,
    pub tools_involved: Vec<String>,
    pub cost: f64,
    pub event_source: PatternEventSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionLostWork {
    pub files: Vec<String>,
    pub bash_count: u64,
    pub edit_count: u64,
    pub read_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionLoss {
    pub session_id: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preceding_message_id: Option<String>,
    pub tokens_before_compact: u64,
    pub cache_lost_cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_work: Option<CompactionLostWork>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditPreview {
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRevertSamplePreview {
    pub first_edit: EditPreview,
    pub revert: EditPreview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRevertCycle {
    pub session_id: String,
    pub file_path: String,
    pub first_edit_turn_index: u64,
    pub revert_turn_index: u64,
    pub span_turns: u64,
    pub cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_preview: Option<EditRevertSamplePreview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRecallDup {
    pub session_id: String,
    pub skill_name: String,
    pub call_count: u64,
    pub first_turn_index: u64,
    pub last_turn_index: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillPruningProtection {
    pub session_id: String,
    pub skill_name: String,
    pub invoked_turn_index: u64,
    pub riding_turns: u64,
    pub last_cached_turn_index: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPromptTax {
    pub session_id: String,
    pub first_turn_cache_create: u64,
    pub first_user_message_tokens: u64,
    pub estimated_system_prompt_tokens: u64,
    pub riding_turns: u64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditHeavySession {
    pub source: SourceKind,
    pub session_id: String,
    pub read_count: u64,
    pub edit_count: u64,
    /// `editCount / readCount`; `f64::INFINITY` when reads === 0.
    pub ratio: f64,
    pub likely_retries: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPatternSummary {
    pub session_id: String,
    pub retry_loop_count: u64,
    pub failure_run_count: u64,
    pub cancellation_run_count: u64,
    pub consecutive_failure_max: u64,
    pub compaction_count: u64,
    pub edit_revert_count: u64,
    pub skill_recall_dup_count: u64,
    pub skill_pruning_protection_count: u64,
    pub system_prompt_tax_count: u64,
    pub edit_heavy_count: u64,
    pub total_retries: u64,
    pub total_pattern_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatternsResult {
    pub retry_loops: Vec<RetryLoop>,
    pub failure_runs: Vec<FailureRun>,
    pub cancelled_runs: Vec<CancellationRun>,
    pub compactions: Vec<CompactionLoss>,
    pub edit_reverts: Vec<EditRevertCycle>,
    pub skill_recall_dups: Vec<SkillRecallDup>,
    pub skill_pruning_protection: Vec<SkillPruningProtection>,
    pub system_prompt_taxes: Vec<SystemPromptTax>,
    pub edit_heavy_sessions: Vec<EditHeavySession>,
    pub session_summaries: Vec<SessionPatternSummary>,
}

// ---------------------------------------------------------------------------
// Finding types
// ---------------------------------------------------------------------------

/// A typed action a finding suggests the user (or `burn hotspots --apply`)
/// take. `Paste` is text the user copies somewhere; `Command` is a shell
/// command; `FileContent` is a full file body to write to disk. Keeping the
/// union closed lets `--apply` decide what is safe to execute automatically
/// vs. what needs explicit user action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WasteAction {
    Paste {
        label: String,
        text: String,
    },
    Command {
        label: String,
        text: String,
    },
    FileContent {
        label: String,
        path: String,
        content: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WasteSeverity {
    Info,
    Warn,
    High,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatedSavings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_session: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_session: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_month: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WasteFinding {
    pub kind: String,
    pub severity: WasteSeverity,
    pub session_id: String,
    pub title: String,
    pub detail: String,
    pub estimated_savings: EstimatedSavings,
    pub actions: Vec<WasteAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_source: Option<PatternEventSource>,
}

// ---------------------------------------------------------------------------
// Adapters
// ---------------------------------------------------------------------------

const SEVERITY_HIGH_USD: f64 = 0.5;
const SEVERITY_WARN_USD: f64 = 0.05;

pub(crate) fn severity_from_usd(usd: f64) -> WasteSeverity {
    if usd >= SEVERITY_HIGH_USD {
        WasteSeverity::High
    } else if usd >= SEVERITY_WARN_USD {
        WasteSeverity::Warn
    } else {
        WasteSeverity::Info
    }
}

pub(crate) fn hotspots_action(session_id: &str) -> WasteAction {
    WasteAction::Command {
        label: "Inspect this session".to_string(),
        text: format!("burn hotspots --session {session_id}"),
    }
}

impl WasteFinding {
    /// Build a cost-driven, session-scoped finding: severity derived from
    /// `cost` via [`severity_from_usd`], a `usd_per_session` saving of `cost`,
    /// and the standard "inspect this session" hotspots action. Adapters that
    /// match this shape construct here and then override `severity`,
    /// `event_source`, or extra savings via the `with_*` chainers below.
    ///
    /// Detectors with bespoke actions or a severity that is decoupled from the
    /// savings figure (ghost surface, tool-output bloat) build the struct
    /// directly rather than overriding every default.
    pub(crate) fn session_cost(
        kind: &str,
        session_id: &str,
        cost: f64,
        title: String,
        detail: String,
    ) -> Self {
        WasteFinding {
            kind: kind.to_string(),
            severity: severity_from_usd(cost),
            session_id: session_id.to_string(),
            title,
            detail,
            estimated_savings: EstimatedSavings {
                usd_per_session: Some(cost),
                ..Default::default()
            },
            actions: vec![hotspots_action(session_id)],
            event_source: None,
        }
    }

    pub(crate) fn with_severity(mut self, severity: WasteSeverity) -> Self {
        self.severity = severity;
        self
    }

    pub(crate) fn with_event_source(mut self, event_source: Option<PatternEventSource>) -> Self {
        self.event_source = event_source;
        self
    }

    pub(crate) fn with_tokens_per_session(mut self, tokens: u64) -> Self {
        self.estimated_savings.tokens_per_session = Some(tokens);
        self
    }
}

use super::util::{fmt_usd, format_with_commas};

pub fn retry_loop_to_finding(loop_: &RetryLoop) -> WasteFinding {
    let target = match &loop_.target {
        Some(t) => format!(" {t}"),
        None => String::new(),
    };
    let title_suffix = match &loop_.error_signature {
        Some(sig) => format!(": '{sig}'"),
        None => String::new(),
    };
    let title = format!(
        "Retry loop: {tool}{target} failed {attempts}× in a row{title_suffix}",
        tool = loop_.tool,
        target = target,
        attempts = loop_.attempts,
        title_suffix = title_suffix,
    );
    let detail = format!(
        "Turns {start}-{end} are {attempts} consecutive errored {tool} calls with the same arguments. \
Cumulative turn cost {cost} — the agent kept retrying without changing inputs.",
        start = loop_.start_turn_index,
        end = loop_.end_turn_index,
        attempts = loop_.attempts,
        tool = loop_.tool,
        cost = fmt_usd(loop_.cost),
    );
    WasteFinding::session_cost("retry-loop", &loop_.session_id, loop_.cost, title, detail)
        .with_event_source(loop_.event_source)
}

pub fn failure_run_to_finding(run: &FailureRun) -> WasteFinding {
    let sig_detail = match &run.error_signatures {
        Some(sigs) if !sigs.is_empty() => {
            let parts: Vec<String> = sigs
                .iter()
                .map(|s| format!("{}='{}'", s.tool, s.first_line))
                .collect();
            format!(" Errors: {}.", parts.join("; "))
        }
        _ => String::new(),
    };
    let title = format!(
        "Failure run: {len} consecutive failed tool calls",
        len = run.length
    );
    let detail = format!(
        "Turns {start}-{end} failed across {n_tools} distinct tool(s) ({tools}). \
Cumulative turn cost {cost} — agent likely stuck without recovering or asking for help.{sig}",
        start = run.start_turn_index,
        end = run.end_turn_index,
        n_tools = run.tools_involved.len(),
        tools = run.tools_involved.join(", "),
        cost = fmt_usd(run.cost),
        sig = sig_detail,
    );
    WasteFinding::session_cost("failure-run", &run.session_id, run.cost, title, detail)
        .with_event_source(run.event_source)
}

pub fn cancellation_run_to_finding(run: &CancellationRun) -> WasteFinding {
    let tool_list = run.tools_involved.join(", ");
    let plural = if run.length == 1 { "" } else { "s" };
    let title = format!(
        "Cancellation run: {len} cancelled tool call{plural}",
        len = run.length,
        plural = plural,
    );
    let detail = format!(
        "Turns {start}-{end} ended with cancelled tool/subagent status ({tools}). \
Cumulative turn cost {cost}.",
        start = run.start_turn_index,
        end = run.end_turn_index,
        tools = tool_list,
        cost = fmt_usd(run.cost),
    );
    WasteFinding::session_cost("cancellation-run", &run.session_id, run.cost, title, detail)
        .with_event_source(Some(run.event_source))
}

pub fn compaction_loss_to_finding(loss: &CompactionLoss) -> WasteFinding {
    let lost_work_detail = match &loss.lost_work {
        Some(work) => {
            let mut s = format!(
                " Compacted window: {edit} edit(s), {bash} bash, {read} read(s)",
                edit = work.edit_count,
                bash = work.bash_count,
                read = work.read_count,
            );
            if !work.files.is_empty() {
                if work.files.len() <= 3 {
                    s.push_str(" on ");
                    s.push_str(&work.files.join(", "));
                } else {
                    s.push_str(" on ");
                    s.push_str(&work.files[..3].join(", "));
                    s.push_str(&format!(" +{} more", work.files.len() - 3));
                }
            }
            s.push('.');
            s
        }
        None => String::new(),
    };
    let title = format!(
        "Compaction lost {tokens} cached tokens",
        tokens = format_with_commas(loss.tokens_before_compact)
    );
    let detail = format!(
        "A compaction at {ts} discarded {tokens} tokens of cache. \
Pre-compact cacheRead cost {cost} — that cache won't be reused on subsequent turns.{lost}",
        ts = loss.ts,
        tokens = format_with_commas(loss.tokens_before_compact),
        cost = fmt_usd(loss.cache_lost_cost),
        lost = lost_work_detail,
    );
    let finding = WasteFinding::session_cost(
        "compaction-loss",
        &loss.session_id,
        loss.cache_lost_cost,
        title,
        detail,
    );
    if loss.tokens_before_compact > 0 {
        finding.with_tokens_per_session(loss.tokens_before_compact)
    } else {
        finding
    }
}

pub fn edit_revert_to_finding(cycle: &EditRevertCycle) -> WasteFinding {
    let preview_detail = match &cycle.sample_preview {
        Some(p) => format!(
            " First edit: '{fe_old}' → '{fe_new}'. Revert: '{r_old}' → '{r_new}'.",
            fe_old = p.first_edit.old,
            fe_new = p.first_edit.new,
            r_old = p.revert.old,
            r_new = p.revert.new,
        ),
        None => String::new(),
    };
    let title = format!("Edit revert on {path}", path = cycle.file_path);
    let detail = format!(
        "Turn {first} edited {path}; turn {revert} restored a prior file state {span} turns later. \
Cumulative anchor-turn cost {cost} — the intermediate work was erased.{preview}",
        first = cycle.first_edit_turn_index,
        path = cycle.file_path,
        revert = cycle.revert_turn_index,
        span = cycle.span_turns,
        cost = fmt_usd(cycle.cost),
        preview = preview_detail,
    );
    WasteFinding::session_cost("edit-revert", &cycle.session_id, cycle.cost, title, detail)
}

pub fn edit_heavy_to_finding(session: &EditHeavySession) -> WasteFinding {
    let ratio_str = if session.ratio.is_finite() {
        format!("{:.1}", session.ratio)
    } else {
        "∞".to_string()
    };
    // Edit-heavy never escalates to High: it is an advisory signal, so cap a
    // High cost-derived severity at Warn.
    let severity = match severity_from_usd(session.cost) {
        WasteSeverity::High => WasteSeverity::Warn,
        other => other,
    };
    // Render the source's kebab-case label so the detail string matches TS's
    // `${session.source}` (which uses the same string set).
    let source_str = session.source.wire_str();
    let title = format!(
        "Edit-heavy session: {edits} edits / {reads} reads (ratio {ratio})",
        edits = session.edit_count,
        reads = session.read_count,
        ratio = ratio_str,
    );
    let detail = format!(
        "{source} session has {edits} edit-tool calls against only {reads} read-tool calls \
(ratio {ratio}, threshold 4×). {retries} edit→bash→edit retry pattern(s) observed. \
Edit-bearing turn cost {cost} — careless editing without first reading surrounding context.",
        source = source_str,
        edits = session.edit_count,
        reads = session.read_count,
        ratio = ratio_str,
        retries = session.likely_retries,
        cost = fmt_usd(session.cost),
    );
    WasteFinding::session_cost(
        "edit-heavy",
        &session.session_id,
        session.cost,
        title,
        detail,
    )
    .with_severity(severity)
}

pub fn skill_recall_dup_to_finding(dup: &SkillRecallDup) -> WasteFinding {
    let title = format!(
        "OpenCode skill \"{name}\" called {count}× without dedup",
        name = dup.skill_name,
        count = dup.call_count,
    );
    let detail = format!(
        "OpenCode does not deduplicate skill tool results, so each of the {count} calls \
(turns {first}-{last}) re-injects the full SKILL.md content into context. \
Cumulative turn cost {cost}.",
        count = dup.call_count,
        first = dup.first_turn_index,
        last = dup.last_turn_index,
        cost = fmt_usd(dup.cost),
    );
    WasteFinding::session_cost("skill-recall-dup", &dup.session_id, dup.cost, title, detail)
}

pub fn skill_pruning_protection_to_finding(prot: &SkillPruningProtection) -> WasteFinding {
    let title = format!(
        "OpenCode skill \"{name}\" rode in cache {turns} turn(s)",
        name = prot.skill_name,
        turns = prot.riding_turns,
    );
    let detail = format!(
        "Skill tool results are listed in OpenCode's PRUNE_PROTECTED_TOOLS and never evict during compaction. \
Invoked at turn {invoked}; still in cacheRead at turn {last}. \
Invoke + riding-turn cost {cost}.",
        invoked = prot.invoked_turn_index,
        last = prot.last_cached_turn_index,
        cost = fmt_usd(prot.cost),
    );
    WasteFinding::session_cost(
        "skill-pruning-protection",
        &prot.session_id,
        prot.cost,
        title,
        detail,
    )
}

pub fn system_prompt_tax_to_finding(tax: &SystemPromptTax) -> WasteFinding {
    let riding_tokens = tax
        .estimated_system_prompt_tokens
        .saturating_mul(tax.riding_turns);
    let title = format!(
        "OpenCode system prompt tax: ~{tokens} tokens × {turns} turn(s)",
        tokens = format_with_commas(tax.estimated_system_prompt_tokens),
        turns = tax.riding_turns,
    );
    let detail = format!(
        "First-turn cacheCreate of {first} tokens minus the first user message ({user}) \
leaves ~{est} tokens of system prompt + skill catalog riding cacheRead across {turns} subsequent turn(s). \
Total cost {cost}.",
        first = format_with_commas(tax.first_turn_cache_create),
        user = format_with_commas(tax.first_user_message_tokens),
        est = format_with_commas(tax.estimated_system_prompt_tokens),
        turns = tax.riding_turns,
        cost = fmt_usd(tax.total_cost),
    );
    WasteFinding::session_cost(
        "system-prompt-tax",
        &tax.session_id,
        tax.total_cost,
        title,
        detail,
    )
    .with_tokens_per_session(riding_tokens)
}

/// Roll the full PatternsResult into a single severity-ranked list. Within
/// the same severity tier, sort by `usdPerSession` descending so the most
/// expensive findings surface first.
pub fn findings_from_patterns(result: &PatternsResult) -> Vec<WasteFinding> {
    let mut findings: Vec<WasteFinding> = Vec::new();
    for r in &result.retry_loops {
        findings.push(retry_loop_to_finding(r));
    }
    for f in &result.failure_runs {
        findings.push(failure_run_to_finding(f));
    }
    for c in &result.cancelled_runs {
        findings.push(cancellation_run_to_finding(c));
    }
    for c in &result.compactions {
        findings.push(compaction_loss_to_finding(c));
    }
    for e in &result.edit_reverts {
        findings.push(edit_revert_to_finding(e));
    }
    for e in &result.edit_heavy_sessions {
        findings.push(edit_heavy_to_finding(e));
    }
    for d in &result.skill_recall_dups {
        findings.push(skill_recall_dup_to_finding(d));
    }
    for p in &result.skill_pruning_protection {
        findings.push(skill_pruning_protection_to_finding(p));
    }
    for s in &result.system_prompt_taxes {
        findings.push(system_prompt_tax_to_finding(s));
    }
    sort_findings(&mut findings);
    findings
}

fn severity_order(s: WasteSeverity) -> u8 {
    match s {
        WasteSeverity::High => 0,
        WasteSeverity::Warn => 1,
        WasteSeverity::Info => 2,
    }
}

/// Sort in place: severity descending (high → warn → info), then by
/// `usdPerSession` descending. Stable, mirroring the TS `Array.prototype.sort`
/// guarantee.
pub fn sort_findings(findings: &mut [WasteFinding]) {
    findings.sort_by(|a, b| {
        let sev = severity_order(a.severity).cmp(&severity_order(b.severity));
        if sev != std::cmp::Ordering::Equal {
            return sev;
        }
        let a_usd = a.estimated_savings.usd_per_session.unwrap_or(0.0);
        let b_usd = b.estimated_savings.usd_per_session.unwrap_or(0.0);
        // Descending: larger first.
        b_usd
            .partial_cmp(&a_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "11111111-2222-3333-4444-555555555555";

    fn base_retry_loop() -> RetryLoop {
        RetryLoop {
            session_id: SESSION.to_string(),
            tool: "Bash".to_string(),
            target: Some("pnpm test".to_string()),
            args_hash: "abc123".to_string(),
            attempts: 4,
            start_turn_index: 0,
            end_turn_index: 3,
            cost: 0.6,
            error_signature: None,
            event_source: None,
        }
    }

    #[test]
    fn retry_loop_to_finding_basic() {
        let f = retry_loop_to_finding(&base_retry_loop());
        assert_eq!(f.kind, "retry-loop");
        assert_eq!(f.session_id, SESSION);
        assert_eq!(f.severity, WasteSeverity::High);
        assert!(
            f.title.contains("Bash pnpm test failed 4× in a row"),
            "title: {}",
            f.title
        );
        assert!(f.detail.contains("Turns 0-3"), "detail: {}", f.detail);
        assert_eq!(f.estimated_savings.usd_per_session, Some(0.6));
        assert_eq!(f.actions.len(), 1);
        match &f.actions[0] {
            WasteAction::Command { text, .. } => {
                assert!(text.contains("burn hotspots --session"), "text: {text}");
            }
            other => panic!("expected Command action, got {other:?}"),
        }
    }

    #[test]
    fn retry_loop_severity_thresholds() {
        let mut r = base_retry_loop();
        r.cost = 0.0001;
        assert_eq!(retry_loop_to_finding(&r).severity, WasteSeverity::Info);
        r.cost = 0.1;
        assert_eq!(retry_loop_to_finding(&r).severity, WasteSeverity::Warn);
        r.cost = 1.0;
        assert_eq!(retry_loop_to_finding(&r).severity, WasteSeverity::High);
    }

    #[test]
    fn failure_run_to_finding_lists_tools() {
        let fr = FailureRun {
            session_id: SESSION.to_string(),
            length: 3,
            start_turn_index: 5,
            end_turn_index: 7,
            tools_involved: vec!["Bash".into(), "Read".into(), "Edit".into()],
            cost: 0.08,
            error_signatures: None,
            event_source: None,
        };
        let f = failure_run_to_finding(&fr);
        assert_eq!(f.kind, "failure-run");
        assert_eq!(f.severity, WasteSeverity::Warn);
        assert!(
            f.detail.contains("Bash, Read, Edit"),
            "detail: {}",
            f.detail
        );
    }

    #[test]
    fn compaction_loss_exposes_tokens() {
        let c = CompactionLoss {
            session_id: SESSION.to_string(),
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            preceding_message_id: Some("msg-1".to_string()),
            tokens_before_compact: 9000,
            cache_lost_cost: 0.04,
            lost_work: None,
        };
        let f = compaction_loss_to_finding(&c);
        assert_eq!(f.kind, "compaction-loss");
        assert_eq!(f.estimated_savings.tokens_per_session, Some(9000));
        assert_eq!(f.estimated_savings.usd_per_session, Some(0.04));
        assert_eq!(f.severity, WasteSeverity::Info);
    }

    #[test]
    fn compaction_loss_omits_tokens_when_zero() {
        let c = CompactionLoss {
            session_id: SESSION.to_string(),
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            preceding_message_id: None,
            tokens_before_compact: 0,
            cache_lost_cost: 0.0,
            lost_work: None,
        };
        let f = compaction_loss_to_finding(&c);
        assert_eq!(f.estimated_savings.tokens_per_session, None);
    }

    #[test]
    fn edit_revert_to_finding_basic() {
        let e = EditRevertCycle {
            session_id: SESSION.to_string(),
            file_path: "src/foo.ts".to_string(),
            first_edit_turn_index: 2,
            revert_turn_index: 8,
            span_turns: 6,
            cost: 0.72,
            sample_preview: None,
        };
        let f = edit_revert_to_finding(&e);
        assert_eq!(f.kind, "edit-revert");
        assert_eq!(f.severity, WasteSeverity::High);
        assert!(f.detail.contains("6 turns later"), "detail: {}", f.detail);
        assert!(f.title.contains("src/foo.ts"), "title: {}", f.title);
    }

    #[test]
    fn edit_heavy_caps_severity_at_warn() {
        let s = EditHeavySession {
            source: SourceKind::ClaudeCode,
            session_id: SESSION.to_string(),
            read_count: 1,
            edit_count: 12,
            ratio: 12.0,
            likely_retries: 3,
            cost: 5.0,
        };
        let f = edit_heavy_to_finding(&s);
        assert_eq!(f.kind, "edit-heavy");
        assert_eq!(f.severity, WasteSeverity::Warn);
        assert!(f.title.contains("12 edits / 1 reads"), "title: {}", f.title);
    }

    #[test]
    fn edit_heavy_renders_infinity_ratio() {
        let s = EditHeavySession {
            source: SourceKind::Codex,
            session_id: SESSION.to_string(),
            read_count: 0,
            edit_count: 6,
            ratio: f64::INFINITY,
            likely_retries: 0,
            cost: 0.01,
        };
        let f = edit_heavy_to_finding(&s);
        assert!(f.title.contains("ratio ∞"), "title: {}", f.title);
    }

    #[test]
    fn skill_recall_dup_basic() {
        let d = SkillRecallDup {
            session_id: SESSION.to_string(),
            skill_name: "init".to_string(),
            call_count: 3,
            first_turn_index: 1,
            last_turn_index: 11,
            cost: 0.2,
        };
        let f = skill_recall_dup_to_finding(&d);
        assert_eq!(f.kind, "skill-recall-dup");
        assert!(
            f.title.contains("init") && f.title.contains("3×"),
            "title: {}",
            f.title
        );
    }

    #[test]
    fn skill_pruning_protection_basic() {
        let p = SkillPruningProtection {
            session_id: SESSION.to_string(),
            skill_name: "init".to_string(),
            invoked_turn_index: 0,
            riding_turns: 7,
            last_cached_turn_index: 7,
            cost: 0.55,
        };
        let f = skill_pruning_protection_to_finding(&p);
        assert_eq!(f.kind, "skill-pruning-protection");
        assert_eq!(f.severity, WasteSeverity::High);
    }

    #[test]
    fn system_prompt_tax_riding_tokens_estimate() {
        let t = SystemPromptTax {
            session_id: SESSION.to_string(),
            first_turn_cache_create: 4500,
            first_user_message_tokens: 500,
            estimated_system_prompt_tokens: 4000,
            riding_turns: 6,
            total_cost: 0.07,
        };
        let f = system_prompt_tax_to_finding(&t);
        assert_eq!(f.kind, "system-prompt-tax");
        assert_eq!(f.estimated_savings.tokens_per_session, Some(4000 * 6));
        assert_eq!(f.estimated_savings.usd_per_session, Some(0.07));
    }

    fn finding_with(kind: &str, sev: WasteSeverity, session: &str, usd: f64) -> WasteFinding {
        WasteFinding {
            kind: kind.to_string(),
            severity: sev,
            session_id: session.to_string(),
            title: kind.to_string(),
            detail: String::new(),
            estimated_savings: EstimatedSavings {
                usd_per_session: Some(usd),
                ..Default::default()
            },
            actions: vec![],
            event_source: None,
        }
    }

    #[test]
    fn sort_findings_orders_high_warn_info_then_usd() {
        let mut findings = vec![
            finding_with("a", WasteSeverity::Info, "s1", 0.001),
            finding_with("b", WasteSeverity::High, "s2", 0.6),
            finding_with("c", WasteSeverity::High, "s3", 1.2),
            finding_with("d", WasteSeverity::Warn, "s4", 0.3),
        ];
        sort_findings(&mut findings);
        let kinds: Vec<&str> = findings.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(kinds, vec!["c", "b", "d", "a"]);
    }

    #[test]
    fn findings_from_patterns_rolls_up_across_kinds() {
        let summary = SessionPatternSummary {
            session_id: SESSION.to_string(),
            retry_loop_count: 1,
            failure_run_count: 1,
            cancellation_run_count: 0,
            consecutive_failure_max: 3,
            compaction_count: 1,
            edit_revert_count: 1,
            skill_recall_dup_count: 0,
            skill_pruning_protection_count: 0,
            system_prompt_tax_count: 0,
            edit_heavy_count: 0,
            total_retries: 4,
            total_pattern_cost: 1.5,
        };
        let result = PatternsResult {
            retry_loops: vec![base_retry_loop()],
            failure_runs: vec![FailureRun {
                session_id: SESSION.to_string(),
                length: 3,
                start_turn_index: 0,
                end_turn_index: 2,
                tools_involved: vec!["Bash".into(), "Edit".into()],
                cost: 0.05,
                error_signatures: None,
                event_source: None,
            }],
            cancelled_runs: vec![],
            compactions: vec![CompactionLoss {
                session_id: SESSION.to_string(),
                ts: "2026-04-20T00:00:00.000Z".to_string(),
                preceding_message_id: Some("m".to_string()),
                tokens_before_compact: 9000,
                cache_lost_cost: 0.04,
                lost_work: None,
            }],
            edit_reverts: vec![EditRevertCycle {
                session_id: SESSION.to_string(),
                file_path: "src/foo.ts".to_string(),
                first_edit_turn_index: 1,
                revert_turn_index: 4,
                span_turns: 3,
                cost: 0.2,
                sample_preview: None,
            }],
            edit_heavy_sessions: vec![],
            skill_recall_dups: vec![],
            skill_pruning_protection: vec![],
            system_prompt_taxes: vec![],
            session_summaries: vec![summary],
        };
        let findings = findings_from_patterns(&result);
        assert_eq!(findings.len(), 4);
        let kinds: std::collections::HashSet<&str> =
            findings.iter().map(|f| f.kind.as_str()).collect();
        assert!(kinds.contains("retry-loop"));
        assert!(kinds.contains("failure-run"));
        assert!(kinds.contains("compaction-loss"));
        assert!(kinds.contains("edit-revert"));
        assert_eq!(findings[0].kind, "retry-loop");
    }

    #[test]
    fn waste_severity_serializes_to_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&WasteSeverity::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&WasteSeverity::Warn).unwrap(),
            "\"warn\""
        );
        assert_eq!(
            serde_json::to_string(&WasteSeverity::Info).unwrap(),
            "\"info\""
        );
    }

    #[test]
    fn waste_action_tag_round_trip() {
        let cmd = WasteAction::Command {
            label: "run".to_string(),
            text: "echo hi".to_string(),
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains("\"type\":\"command\""), "json: {s}");
        let fc = WasteAction::FileContent {
            label: "write".to_string(),
            path: "a.txt".to_string(),
            content: "x".to_string(),
        };
        let s = serde_json::to_string(&fc).unwrap();
        assert!(s.contains("\"type\":\"file-content\""), "json: {s}");
    }

    #[test]
    fn format_with_commas_basic() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(100), "100");
        assert_eq!(format_with_commas(1000), "1,000");
        assert_eq!(format_with_commas(1234567), "1,234,567");
        assert_eq!(format_with_commas(9000), "9,000");
    }
}
