//! Ingest orchestration — Rust port of `packages/ingest/src/ingest.ts`.
//!
//! Owns the public verb surface (`ingest_all`, the per-harness verbs, and
//! `reingest_missing_content`), the [`IngestReport`] / [`IngestOptions`]
//! types, and the per-harness orchestration loops.
//!
//! ## Status
//!
//! The standalone modules of this crate (`pending_stamps`, `walk`,
//! `watch_loop`, `gap`, `reingest`) are fully ported and tested. The
//! gap-warning state machine (`gap`) and `reingest_missing_content`
//! (`reingest`) landed in #278 and depend on the freshly-ported
//! `Ledger::list_content_session_ids` / `Ledger::list_user_turn_session_ids`
//! plus `relayburn_ledger::load_config` from #279.
//!
//! The per-harness orchestration helpers below
//! (`ingest_claude_into`, `ingest_codex_into`, `ingest_opencode_into`,
//! and the `ingest_claude_session` fast path) are scaffolded — wiring
//! the parser surface into them lands in #277. Until then they return
//! empty reports rather than half-implementing the parse+append loop.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use relayburn_ledger::Ledger;

use crate::cursors::{load_cursors, save_cursor_changes};
use crate::pending_stamps::cleanup_stale_pending_stamps;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestReport {
    pub scanned_sessions: usize,
    pub ingested_sessions: usize,
    pub appended_turns: usize,
}

impl IngestReport {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: &IngestReport) {
        self.scanned_sessions += other.scanned_sessions;
        self.ingested_sessions += other.ingested_sessions;
        self.appended_turns += other.appended_turns;
    }
}

/// Options shared across every public ingest verb. Mirrors the TS
/// `IngestOptions` shape so callers can hand the same struct to each verb.
/// Sink for short orchestration progress strings (one per phase). The CLI
/// hook surface uses this to drive a spinner; default ingest leaves it unset.
pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync>;

/// Sink for content-capture gap warnings. The TS adapter routes these
/// through an active spinner; the Rust port leaves the routing decision to
/// the caller.
pub type WarnSink = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct IngestOptions {
    pub on_progress: Option<ProgressSink>,
    pub on_warn: Option<WarnSink>,
    /// Override for the upstream session-store layout. Defaults to the
    /// per-harness home dirs (`~/.claude/projects`, `~/.codex/sessions`,
    /// `~/.local/share/opencode/storage`).
    pub roots: IngestRoots,
}

/// Per-harness root paths. `None` means "use the OS default for this harness".
/// Tests inject explicit roots so they don't scan the developer's home dir.
#[derive(Debug, Clone, Default)]
pub struct IngestRoots {
    pub claude_projects_dir: Option<PathBuf>,
    pub codex_sessions_dir: Option<PathBuf>,
    pub opencode_storage_dir: Option<PathBuf>,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn claude_projects_dir(roots: &IngestRoots) -> PathBuf {
    roots
        .claude_projects_dir
        .clone()
        .unwrap_or_else(|| home_dir().join(".claude").join("projects"))
}

#[allow(dead_code)]
pub(crate) fn codex_sessions_dir(roots: &IngestRoots) -> PathBuf {
    roots
        .codex_sessions_dir
        .clone()
        .unwrap_or_else(|| home_dir().join(".codex").join("sessions"))
}

pub(crate) fn opencode_storage_dir(roots: &IngestRoots) -> PathBuf {
    roots.opencode_storage_dir.clone().unwrap_or_else(|| {
        home_dir()
            .join(".local")
            .join("share")
            .join("opencode")
            .join("storage")
    })
}

#[allow(dead_code)]
pub(crate) fn opencode_session_root(roots: &IngestRoots) -> PathBuf {
    opencode_storage_dir(roots).join("session")
}

#[allow(dead_code)]
pub(crate) fn opencode_message_root(roots: &IngestRoots) -> PathBuf {
    opencode_storage_dir(roots).join("message")
}

fn progress(opts: &IngestOptions, msg: &str) {
    if let Some(cb) = &opts.on_progress {
        cb(msg);
    }
}

/// Ingest every known session store once. Cleans stale pending stamps,
/// loads cursors, walks Claude/Codex/OpenCode in turn, then persists any
/// cursor mutations. Returns the merged report.
pub async fn ingest_all(ledger: &mut Ledger, opts: &IngestOptions) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps()?;
    progress(opts, "loading ingest cursors");
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let mut report = IngestReport::empty();

    progress(opts, "scanning Claude Code sessions");
    let r = ingest_claude_into(ledger, &mut after, &opts.roots).await?;
    report.merge(&r);
    progress(opts, "scanning Codex sessions");
    let r = ingest_codex_into(ledger, &mut after, &opts.roots).await?;
    report.merge(&r);
    progress(opts, "scanning OpenCode sessions");
    let r = ingest_opencode_into(ledger, &mut after, &opts.roots).await?;
    report.merge(&r);

    progress(opts, "saving ingest cursors");
    save_cursor_changes(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

pub async fn ingest_claude_projects(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps()?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let report = ingest_claude_into(ledger, &mut after, &opts.roots).await?;
    save_cursor_changes(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

pub async fn ingest_codex_sessions(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps()?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let report = ingest_codex_into(ledger, &mut after, &opts.roots).await?;
    save_cursor_changes(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

pub async fn ingest_opencode_sessions(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps()?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let report = ingest_opencode_into(ledger, &mut after, &opts.roots).await?;
    save_cursor_changes(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

/// Per-session fast-path used by the claude harness adapter after a
/// `burn run` exits. Caller already knows the sessionId from the spawn
/// plan, so we go straight to the one JSONL file and persist a cursor at
/// EOF — a later `ingest_all` sweep then skips it.
pub async fn ingest_claude_session(
    ledger: &mut Ledger,
    cwd: &str,
    session_id: &str,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    let _ = (ledger, opts);
    // Encode cwd → flattened dir name (TS: `cwd.replace(/\//g, '-')`).
    let encoded: String = cwd
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let file = claude_projects_dir(&opts.roots)
        .join(&encoded)
        .join(format!("{session_id}.jsonl"));
    if !file.is_file() {
        return Ok(IngestReport::empty());
    }
    // The full implementation invokes `parse_claude_session` from
    // `relayburn_reader` and appends turns + content + cursor. Pending
    // wiring through the per-adapter helpers below.
    Ok(IngestReport {
        scanned_sessions: 1,
        ingested_sessions: 0,
        appended_turns: 0,
    })
}

// --- per-harness orchestration -----------------------------------------

async fn ingest_claude_into(
    _ledger: &mut Ledger,
    _cursors: &mut crate::cursors::Cursors,
    _roots: &IngestRoots,
) -> anyhow::Result<IngestReport> {
    Ok(IngestReport::empty())
}

async fn ingest_codex_into(
    _ledger: &mut Ledger,
    _cursors: &mut crate::cursors::Cursors,
    _roots: &IngestRoots,
) -> anyhow::Result<IngestReport> {
    Ok(IngestReport::empty())
}

async fn ingest_opencode_into(
    _ledger: &mut Ledger,
    _cursors: &mut crate::cursors::Cursors,
    _roots: &IngestRoots,
) -> anyhow::Result<IngestReport> {
    Ok(IngestReport::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_merge_sums_components() {
        let mut a = IngestReport {
            scanned_sessions: 1,
            ingested_sessions: 2,
            appended_turns: 3,
        };
        let b = IngestReport {
            scanned_sessions: 10,
            ingested_sessions: 20,
            appended_turns: 30,
        };
        a.merge(&b);
        assert_eq!(a.scanned_sessions, 11);
        assert_eq!(a.ingested_sessions, 22);
        assert_eq!(a.appended_turns, 33);
    }

    #[test]
    fn roots_default_to_home_layout() {
        let roots = IngestRoots::default();
        let claude = claude_projects_dir(&roots);
        let codex = codex_sessions_dir(&roots);
        let opencode = opencode_storage_dir(&roots);
        assert!(claude.ends_with(".claude/projects"));
        assert!(codex.ends_with(".codex/sessions"));
        assert!(opencode.ends_with(".local/share/opencode/storage"));
    }

    #[test]
    fn roots_overrides_take_priority() {
        let roots = IngestRoots {
            claude_projects_dir: Some(PathBuf::from("/x/claude")),
            codex_sessions_dir: Some(PathBuf::from("/x/codex")),
            opencode_storage_dir: Some(PathBuf::from("/x/oc")),
        };
        assert_eq!(claude_projects_dir(&roots), PathBuf::from("/x/claude"));
        assert_eq!(codex_sessions_dir(&roots), PathBuf::from("/x/codex"));
        assert_eq!(opencode_storage_dir(&roots), PathBuf::from("/x/oc"));
    }
}
