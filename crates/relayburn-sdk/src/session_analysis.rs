//! `analyze_session` — every burn analyzer over one session, without a
//! ledger or ingest.
//!
//! relayhistory reads the session ([`SessionLocator`]); burn maps the
//! evidence onto its turn records and runs its accounting, pricing and
//! analyzers in memory. The result is one versioned document,
//! [`SessionAnalysis`] (`burn.session-analysis.v1`).
//!
//! ```no_run
//! use relayburn_sdk::{analyze_session, AnalyzeSessionOptions, Harness, SessionLocator};
//!
//! let analysis = analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
//!     harness: Harness::ClaudeCode,
//!     path: "/path/to/session.jsonl".into(),
//! }))?;
//! for finding in &analysis.findings {
//!     println!("{}: {}", finding.code, finding.suggestion);
//! }
//! # Ok::<_, anyhow::Error>(())
//! ```

use std::path::{Path, PathBuf};

use ai_hist::{ProviderRoots, SessionEvidence};
use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::analyze::{build_ghost_surface_inputs, load_pricing, PricingTable};
use crate::analyze::{load_claude_settings, project_claude_settings_path, LoadedClaudeSettings};
use crate::query_verbs::{default_hotspots_finding_kinds, detect_hotspots, HotspotEnvironment};
use crate::reader::Harness;
use crate::source::locate::{load_session, HistoryStoreOptions, SessionLocator};
use crate::source::{records_with_children, SessionRecords};

mod document;
mod explain;
mod findings;
mod reasoning;
mod sections;
mod structure;

pub use document::*;
pub use reasoning::{
    reasoning_effort_rows, ReasoningBreakdown, ReasoningEffortChange, ReasoningEffortRow,
};

/// What [`analyze_session`] reads.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeSessionOptions {
    pub session: SessionLocator,
    /// Store that resolves [`SessionLocator::Id`]; ignored for paths.
    #[serde(default)]
    pub store: HistoryStoreOptions,
    /// models.dev-format pricing overlaid on the built-in table.
    #[serde(default)]
    pub pricing_path: Option<PathBuf>,
    /// Project whose instruction files the overhead section prices.
    /// Defaults to the session's recorded working directory.
    #[serde(default)]
    pub project_dir: Option<PathBuf>,
}

impl AnalyzeSessionOptions {
    pub fn new(session: SessionLocator) -> Self {
        Self {
            session,
            store: HistoryStoreOptions::default(),
            pricing_path: None,
            project_dir: None,
        }
    }
}

/// What [`analyze_evidence`] needs besides the evidence itself.
#[derive(Debug, Clone, Default)]
pub struct AnalysisSettings {
    pub pricing_path: Option<PathBuf>,
    /// Project whose instruction files the overhead section prices.
    /// Defaults to the session's recorded working directory.
    pub project_dir: Option<PathBuf>,
    /// Provider roots of the harness install the session ran under. The
    /// installed-surface (ghost-surface) and settings checks need them.
    pub roots: Option<ProviderRoots>,
    /// Evidence of the Claude subagents the session delegated work to
    /// (each [`ai_hist::SessionStore::delegated_descendants`] id read with
    /// [`ai_hist::SessionStore::session`]). Their turns are billed and
    /// analyzed as subagent work of the session.
    pub delegated_children: Vec<SessionEvidence>,
}

/// Analyze one session: resolve it through relayhistory, then run every
/// analyzer over it.
pub fn analyze_session(opts: AnalyzeSessionOptions) -> Result<SessionAnalysis> {
    let loaded = load_session(&opts.session, &opts.store)?;
    let mut analysis = analyze_evidence(
        &loaded.evidence,
        &AnalysisSettings {
            pricing_path: opts.pricing_path,
            project_dir: opts.project_dir,
            roots: loaded.roots.clone(),
            delegated_children: loaded.children,
        },
    )?;
    if let SessionLocator::Path { path, .. } = &opts.session {
        analysis.session.transcript_path = Some(path.to_string_lossy().into_owned());
    }
    Ok(analysis)
}

/// Analyze relayhistory evidence an embedder already holds (for example
/// from its own [`ai_hist::SessionStore`]).
pub fn analyze_evidence(
    evidence: &SessionEvidence,
    settings: &AnalysisSettings,
) -> Result<SessionAnalysis> {
    let harness = harness_of(evidence.session.source).ok_or_else(|| {
        anyhow!(
            "burn analyzes claude, codex and opencode sessions, not {}",
            evidence.session.source
        )
    })?;
    let children = &settings.delegated_children;
    let records = records_with_children(evidence, children);
    let pricing = load_pricing(settings.pricing_path.as_deref());
    let inputs = Inputs {
        harness,
        evidence,
        children,
        records: &records,
        pricing: &pricing,
        project_dir: settings
            .project_dir
            .clone()
            .or_else(|| evidence.session.cwd.as_ref().map(PathBuf::from)),
        roots: settings.roots.as_ref(),
    };
    Ok(analyze(&inputs))
}

fn harness_of(source: ai_hist::Source) -> Option<Harness> {
    match source {
        ai_hist::Source::Claude => Some(Harness::ClaudeCode),
        ai_hist::Source::Codex => Some(Harness::Codex),
        ai_hist::Source::OpenCode => Some(Harness::Opencode),
        _ => None,
    }
}

/// One session's records and the environment its analyzers read.
pub(crate) struct Inputs<'a> {
    harness: Harness,
    evidence: &'a SessionEvidence,
    /// The delegated subagents folded into `records`.
    children: &'a [SessionEvidence],
    records: &'a SessionRecords,
    pricing: &'a PricingTable,
    project_dir: Option<PathBuf>,
    roots: Option<&'a ProviderRoots>,
}

impl Inputs<'_> {
    /// The project directory, or why there is none to read.
    fn project_dir(&self) -> Result<&Path, String> {
        let dir = self.project_dir.as_deref().ok_or_else(|| {
            "the session recorded no working directory and no project directory was given"
                .to_string()
        })?;
        if dir.is_dir() {
            Ok(dir)
        } else {
            Err(format!(
                "project directory {} does not exist on this machine",
                dir.display()
            ))
        }
    }
}

fn analyze(inputs: &Inputs<'_>) -> SessionAnalysis {
    let turns = &inputs.records.turns;
    let trees = structure::span_trees(inputs);
    let hotspots = structure::hotspots(inputs);
    let overhead = structure::overhead(inputs);
    let context = structure::context(inputs, &trees);
    let (environment_settings, skipped_checks) = environment(inputs);
    let wanted = default_hotspots_finding_kinds().into_iter().collect();
    let detections = detect_hotspots(
        turns,
        &structure::side_records(inputs),
        inputs.pricing,
        &HotspotEnvironment {
            settings: &environment_settings.settings,
            ghost_surface: environment_settings.ghost_surface,
            wanted: &wanted,
        },
    );
    let refusal = match &hotspots {
        Section::Unavailable { reason } if !turns.is_empty() => Some(reason.as_str()),
        _ => None,
    };
    let reasoning = reasoning::breakdown(inputs.harness, turns, inputs.pricing);
    let findings = findings::findings(
        inputs,
        &detections,
        &findings::Reports {
            overhead: &overhead,
            context: &context,
            reasoning: &reasoning,
        },
        refusal,
    );
    SessionAnalysis {
        schema: SESSION_ANALYSIS_SCHEMA.to_string(),
        session: sections::identity(inputs),
        fidelity: sections::fidelity(inputs),
        metrics: match crate::session_metrics::session_metrics(
            inputs.harness,
            turns,
            inputs.pricing,
        ) {
            Ok(metrics) => Section::available(metrics),
            Err(error) => Section::unavailable(error.to_string()),
        },
        activity: sections::activity(inputs),
        reasoning,
        hotspots,
        overhead,
        subagents: structure::subagents(inputs),
        flow: structure::flow(inputs, &trees),
        context,
        quality: sections::quality(inputs),
        stop_reasons: sections::stop_reasons(inputs),
        findings,
        skipped_checks,
    }
}

struct EnvironmentInputs {
    settings: Vec<LoadedClaudeSettings>,
    ghost_surface: Option<crate::analyze::ghost_surface::GhostSurfaceInputs>,
}

/// Settings and installed-surface inputs from the harness install and
/// project, plus the checks that cannot run without them.
fn environment(inputs: &Inputs<'_>) -> (EnvironmentInputs, Vec<SkippedCheck>) {
    let project = inputs.project_dir().ok();
    let mut settings: Vec<LoadedClaudeSettings> = Vec::new();
    if let Some(roots) = inputs.roots {
        settings.extend(load_claude_settings(roots.claude.join("settings.json")));
    }
    if let Some(dir) = project {
        settings.extend(load_claude_settings(project_claude_settings_path(dir)));
    }
    let mut skipped = Vec::new();
    let ghost_surface = match inputs.roots {
        Some(roots) => {
            let mut ghost = build_ghost_surface_inputs(&inputs.records.turns, inputs.pricing, None);
            ghost.claude_home = Some(roots.claude.clone());
            ghost.codex_home = Some(roots.codex.clone());
            ghost.opencode_projects = Some(vec![project
                .map(Path::to_path_buf)
                .unwrap_or_else(|| roots.home.clone())]);
            Some(ghost)
        }
        None => {
            skipped.push(SkippedCheck {
                check: "ghost-surface".to_string(),
                reason: "the session was read outside a harness install, so its installed skills, agents and commands are unknown".to_string(),
            });
            None
        }
    };
    (
        EnvironmentInputs {
            settings,
            ghost_surface,
        },
        skipped,
    )
}

#[cfg(test)]
mod tests;
