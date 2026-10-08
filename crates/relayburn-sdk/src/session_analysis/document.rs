//! The `burn.session-analysis.v1` document.

use serde::{Deserialize, Serialize};

use crate::analyze::{
    ContextDelta, FindingPricingStatus, OneShotMetrics, ReplacementSavingsSummary, SessionOutcome,
    SubagentTreeNode, WasteAction, WasteSeverity,
};
use crate::reader::{ActivityCategory, CompactionEvent, Harness};
use crate::{
    HotspotsAttributionResult, OverheadResult, OverheadTrimResult, SessionMetrics, StopReasonCounts,
};

pub const SESSION_ANALYSIS_SCHEMA: &str = "burn.session-analysis.v1";

/// Everything burn can say about one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAnalysis {
    /// Always [`SESSION_ANALYSIS_SCHEMA`].
    pub schema: String,
    pub session: SessionIdentity,
    pub fidelity: FidelityReport,
    /// Token and cost totals per model (`burn.session-metrics.v1`).
    pub metrics: Section<SessionMetrics>,
    pub activity: Section<ActivityBreakdown>,
    /// Session cost attributed to files, commands, subagents and MCP servers.
    pub hotspots: Section<HotspotsAttributionResult>,
    /// Cost of the project instruction files the session loaded.
    pub overhead: Section<OverheadReport>,
    pub subagents: Section<SubagentTreeNode>,
    pub flow: Section<FlowSummary>,
    pub context: Section<ContextReport>,
    pub quality: Section<QualityReport>,
    pub stop_reasons: Section<StopReasonCounts>,
    /// Diagnoses, most severe and most expensive first.
    pub findings: Vec<Finding>,
    /// Finding checks that did not run, and why.
    pub skipped_checks: Vec<SkippedCheck>,
}

/// One analysis section: its data, or why burn could not produce it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum Section<T> {
    Available { data: T },
    Unavailable { reason: String },
}

impl<T> Section<T> {
    pub fn available(data: T) -> Self {
        Self::Available { data }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn data(&self) -> Option<&T> {
        match self {
            Self::Available { data } => Some(data),
            Self::Unavailable { .. } => None,
        }
    }

    /// Why the section is unavailable; `None` when it is available.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available { .. } => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdentity {
    pub harness: Harness,
    pub session_id: String,
    /// The artifact analyzed: the caller's path, else where relayhistory
    /// found the transcript.
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub agent_version: Option<String>,
    pub models: Vec<String>,
    pub first_activity: Option<String>,
    pub last_activity: Option<String>,
    pub turn_count: u64,
    pub user_turn_count: u64,
    pub tool_call_count: u64,
    pub compaction_count: u64,
}

/// How much of the session's accounting the transcript supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FidelityReport {
    /// `{ total, byClass, byGranularity, missingCoverage, unknown }` over
    /// the session's turns.
    pub summary: serde_json::Value,
    /// Turns with the tool-call and tool-result coverage cost attribution
    /// needs.
    pub attributable_turns: u64,
    /// relayhistory evidence kinds the harness can capture.
    pub evidence_kinds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityBreakdown {
    /// Turns per classified activity, most expensive first.
    pub categories: Vec<ActivityRow>,
    /// Tool calls per tool. `tokens` / `costUsd` are those of the turns that
    /// called the tool, so a turn calling two tools counts toward both.
    pub tools: Vec<ToolRow>,
    pub replacement_savings: Option<ReplacementSavingsSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRow {
    /// `None` for turns the classifier left unlabeled.
    pub category: Option<ActivityCategory>,
    pub turns: u64,
    pub tokens: u64,
    /// `None` when any contributing turn's model is unpriced.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRow {
    pub tool: String,
    pub calls: u64,
    pub errors: u64,
    pub turns: u64,
    pub tokens: u64,
    /// `None` when any contributing turn's model is unpriced.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadReport {
    pub project_dir: String,
    pub attribution: OverheadResult,
    pub trim: OverheadTrimResult,
}

/// Shape of the session's inference-flow DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowSummary {
    pub turns: u64,
    pub inferences: u64,
    pub tool_uses: u64,
    pub subagents: u64,
    pub skills: u64,
    /// Main rail plus one per dispatched subagent.
    pub rails: u64,
    pub edges: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextReport {
    /// Largest context-window size any turn sent (input + cache read +
    /// cache write).
    pub peak_context_tokens: u64,
    pub peak_turn_id: Option<String>,
    pub compactions: Vec<CompactionEvent>,
    /// Largest per-inference context growths, with what caused them.
    pub largest_growth: Vec<ContextDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityReport {
    pub outcome: Option<SessionOutcome>,
    pub one_shot: Option<OneShotMetrics>,
}

/// One diagnosis: what happened, why it costs tokens, and what to change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// Stable identifier (`retry-loop`, `tool-output-bloat`, …).
    pub code: String,
    pub severity: WasteSeverity,
    pub title: String,
    /// What happened in this session and why it costs tokens.
    pub explanation: String,
    pub evidence: FindingEvidence,
    pub impact: FindingImpact,
    /// The concrete change that avoids the cost.
    pub suggestion: String,
    pub actions: Vec<WasteAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingEvidence {
    pub turn_ids: Vec<String>,
    pub turn_indexes: Vec<u64>,
    pub tools: Vec<String>,
    pub files: Vec<String>,
    /// Commands, skills, sections or other targets the finding concerns.
    pub targets: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingImpact {
    /// Tokens the pattern cost or would save; for detectors that report
    /// only USD, the tokens of the turns in the evidence.
    pub tokens: Option<u64>,
    /// `None` when the cost is unpriced or not expressible in USD.
    pub cost_usd: Option<f64>,
    pub pricing: FindingPricingStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedCheck {
    pub check: String,
    pub reason: String,
}
