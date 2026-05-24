//! Ingest orchestration — Rust port of `packages/ingest/src/ingest.ts`.
//!
//! Owns the public verb surface (`ingest_all`, the per-harness verbs, and
//! `reingest_missing_content`), the [`IngestReport`] / [`IngestOptions`]
//! types, and the per-harness orchestration loops.
//!
//! ## Status
//!
//! The standalone modules of this crate (`pending_stamps`, `walk`,
//! `watch_loop`, `cursors`, `gap`, `reingest`) are fully ported and tested.
//! The per-harness orchestration helpers below are filled in (#277):
//! `ingest_claude_into`, `ingest_codex_into`, `ingest_opencode_into`, plus
//! the `ingest_claude_session` fast-path. The gap-warning state machine
//! (`gap`) and `reingest_missing_content` (`reingest`) landed in #278 and
//! depend on the freshly-ported `Ledger::list_content_session_ids` /
//! `Ledger::list_user_turn_session_ids` plus `crate::ledger::load_config`
//! from #279.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ledger::{load_config, Ledger};
use crate::reader::{
    parse_claude_session, parse_claude_session_incremental, parse_codex_session_incremental,
    parse_opencode_session_incremental, reconcile_claude_session_relationships,
    ClaudeParseIncrementalOptions, ClaudeParseIncrementalResult, ClaudeParseOptions,
    ClaudeParseResult, CodexLastCompletedTurn, CodexResumeState, CodexTurnContext, CompactionEvent,
    ContentRecord, ContentStoreMode, CumulativeUsage as ReaderCumulativeUsage,
    ParseCodexIncrementalOptions, ParseCodexIncrementalResult, ParseOpencodeIncrementalOptions,
    ParseOpencodeIncrementalResult, PersistedUserTurnSlot, ReconcileClaudeRelationshipsInput,
    SessionRelationshipRecord, ToolResultEventRecord, UserTurnRecord,
};

use crate::ingest::cursors::{
    load_cursors, save_cursors_if_changed, ClaudeCursor, CodexCumulative, CodexCursor, Cursors,
    FileCursor, OpencodeCursor,
};
use crate::ingest::gap::{
    count_new_tool_calls, count_new_tool_results, emit_gap_warning, record_session_gap, AdapterName,
};
use crate::ingest::pending_stamps::{
    cleanup_stale_pending_stamps_in, resolve_pending_stamps_for_session_in, PendingStampHarness,
    PendingStampSessionCandidate,
};
use crate::ingest::reingest::derive_codex_session_id;
use crate::ingest::walk::{list_dirs, list_jsonl_files, walk_jsonl, walk_opencode_sessions};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestReport {
    pub scanned_sessions: usize,
    pub ingested_sessions: usize,
    pub appended_turns: usize,
    #[serde(default)]
    pub applied_pending_stamps: usize,
}

impl IngestReport {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: &IngestReport) {
        self.scanned_sessions += other.scanned_sessions;
        self.ingested_sessions += other.ingested_sessions;
        self.appended_turns += other.appended_turns;
        self.applied_pending_stamps += other.applied_pending_stamps;
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
    /// Override for the relayburn home used by sidecar ingest state such as
    /// config and pending-stamp manifests. The opened [`Ledger`] still owns
    /// the actual database paths.
    pub ledger_home: Option<PathBuf>,
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

pub(crate) fn opencode_session_root(roots: &IngestRoots) -> PathBuf {
    opencode_storage_dir(roots).join("session")
}

/// Resolve the default session-store roots ingest scans, in the same
/// order `ingest_all` walks them. Used by the watch loop to drive its
/// `notify`-backed FS-event driver against the harness home dirs the
/// SDK already owns. Test injection: pass an explicit
/// [`IngestRoots`] to override individual paths; defaults still come
/// from `$HOME` for fields left `None`.
///
/// Returns the Claude / Codex / OpenCode roots in that order — the
/// caller doesn't have to filter for existence; the FS-event driver
/// silently skips any path that doesn't yet exist.
pub fn default_session_roots(roots: &IngestRoots) -> Vec<PathBuf> {
    vec![
        claude_projects_dir(roots),
        codex_sessions_dir(roots),
        opencode_storage_dir(roots),
    ]
}

pub(crate) fn opencode_message_root(roots: &IngestRoots) -> PathBuf {
    opencode_storage_dir(roots).join("message")
}

fn progress(opts: &IngestOptions, msg: &str) {
    if let Some(cb) = &opts.on_progress {
        cb(msg);
    }
}

/// Resolve `content.store` from `$RELAYBURN_HOME/config.json` (with env
/// overrides). Mirrors the TS `resolveContentMode`. Falls back to
/// `ContentStoreMode::Full` if the config layer errors — keeps ingest
/// resilient against a corrupt config file.
fn resolve_content_mode(ledger_home: Option<&Path>) -> ContentStoreMode {
    let config = match ledger_home {
        Some(home) => crate::ledger::load_config_at(&home.join("config.json")),
        None => load_config(),
    };
    config
        .map(|c| c.content.store)
        .unwrap_or(ContentStoreMode::Full)
}

/// Ingest every known session store once. Cleans stale pending stamps,
/// loads cursors, walks Claude/Codex/OpenCode in turn, then persists any
/// cursor mutations. Returns the merged report.
pub fn ingest_all(ledger: &mut Ledger, opts: &IngestOptions) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;
    progress(opts, "loading ingest cursors");
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let mut report = IngestReport::empty();

    progress(opts, "loading content settings");
    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    let on_warn: Option<&dyn Fn(&str)> = opts.on_warn.as_ref().map(|f| f.as_ref() as &dyn Fn(&str));

    // Emit per-adapter, immediately after each scan, so a later adapter
    // returning Err does not swallow a gap the earlier adapter already
    // recorded against work that was already appended.
    progress(opts, "scanning Claude Code sessions");
    let r = ingest_claude_into(
        ledger,
        &mut after,
        &opts.roots,
        content_mode,
        opts.ledger_home.as_deref(),
    )?;
    report.merge(&r);
    emit_gap_warning(AdapterName::Claude, content_mode, on_warn);

    progress(opts, "scanning Codex sessions");
    let r = ingest_codex_into(
        ledger,
        &mut after,
        &opts.roots,
        content_mode,
        opts.ledger_home.as_deref(),
    )?;
    report.merge(&r);
    emit_gap_warning(AdapterName::Codex, content_mode, on_warn);

    progress(opts, "scanning OpenCode sessions");
    let r = ingest_opencode_into(
        ledger,
        &mut after,
        &opts.roots,
        content_mode,
        opts.ledger_home.as_deref(),
    )?;
    report.merge(&r);
    emit_gap_warning(AdapterName::Opencode, content_mode, on_warn);

    progress(opts, "saving ingest cursors");
    save_cursors_if_changed(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

#[cfg(test)]
pub fn ingest_claude_projects(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    run_single_harness(ledger, opts, AdapterName::Claude, ingest_claude_into)
}

pub fn ingest_codex_sessions(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    run_single_harness(ledger, opts, AdapterName::Codex, ingest_codex_into)
}

pub fn ingest_opencode_sessions(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    run_single_harness(ledger, opts, AdapterName::Opencode, ingest_opencode_into)
}

/// Shared boilerplate for the per-harness verbs: clean stale stamps, snapshot
/// cursors, resolve content mode, run the harness body, emit any pending gap
/// warning for that adapter, then persist cursor mutations. The per-harness
/// `ingest_*_into` functions plug straight in as `body`.
fn run_single_harness<F>(
    ledger: &mut Ledger,
    opts: &IngestOptions,
    adapter: AdapterName,
    body: F,
) -> anyhow::Result<IngestReport>
where
    F: FnOnce(
        &mut Ledger,
        &mut Cursors,
        &IngestRoots,
        ContentStoreMode,
        Option<&Path>,
    ) -> anyhow::Result<IngestReport>,
{
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    let report = body(
        ledger,
        &mut after,
        &opts.roots,
        content_mode,
        opts.ledger_home.as_deref(),
    )?;
    let on_warn: Option<&dyn Fn(&str)> = opts.on_warn.as_ref().map(|f| f.as_ref() as &dyn Fn(&str));
    emit_gap_warning(adapter, content_mode, on_warn);
    save_cursors_if_changed(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    Ok(report)
}

/// Per-session fast-path used when a Claude launcher already knows the
/// sessionId from the spawn plan. We go straight to the one JSONL file and
/// persist a cursor at EOF — a later `ingest_all` sweep then skips it.
pub fn ingest_claude_session(
    ledger: &mut Ledger,
    cwd: &str,
    session_id: &str,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    // Encode cwd → flattened dir name (TS: `cwd.replace(/\//g, '-')`).
    let encoded = cwd.replace('/', "-");
    let file = claude_projects_dir(&opts.roots)
        .join(&encoded)
        .join(format!("{session_id}.jsonl"));
    match fs::metadata(&file) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Ok(IngestReport::empty()),
        Err(_) => {
            eprintln!("[burn] no session file found at {}", file.display());
            return Ok(IngestReport::empty());
        }
    }

    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    let parse_opts = ClaudeParseOptions {
        session_path: Some(file.to_string_lossy().into_owned()),
        content_mode: Some(content_mode),
        ..Default::default()
    };
    let result = parse_claude_session(&file, &parse_opts).map_err(|e| anyhow::anyhow!(e))?;
    if result.turns.is_empty() {
        return Ok(IngestReport {
            scanned_sessions: 1,
            ingested_sessions: 0,
            appended_turns: 0,
            applied_pending_stamps: 0,
        });
    }

    let appended_turns = result.turns.len();
    ledger.append_turns(&result.turns)?;
    apply_parsed_extras(ledger, &result)?;

    // Re-stat after parsing so the cursor reflects the byte position the
    // parser actually read to. `parse_claude_session` uses BufReader::lines()
    // and keeps reading past the pre-parse `len()` if the file grew during
    // parse; using the pre-parse size would cause a follow-up `ingest_all`
    // to replay those bytes and emit duplicate turns.
    let meta = fs::metadata(&file)?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let cursor = ClaudeCursor {
        inode: file_inode(&meta),
        offset_bytes: meta.len(),
        mtime_ms: mtime_ms(&meta),
        last_user_text: None,
    };
    let key = file.to_string_lossy().into_owned();
    after.insert(key, FileCursor::Claude(cursor));
    save_cursors_if_changed(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;

    Ok(IngestReport {
        scanned_sessions: 1,
        ingested_sessions: 1,
        appended_turns,
        applied_pending_stamps: 0,
    })
}

// --- per-harness orchestration -----------------------------------------

/// Iterate every project directory in `~/.claude/projects/` (or the test
/// override), run `parse_claude_session_incremental` per JSONL file with
/// per-file cursor + lastUserText carry-over, append turns/content/events/
/// relationships/toolResultEvents/userTurns, then run a single cross-file
/// reconciliation step at the end.
fn ingest_claude_into(
    ledger: &mut Ledger,
    cursors: &mut Cursors,
    roots: &IngestRoots,
    content_mode: ContentStoreMode,
    ledger_home: Option<&Path>,
) -> anyhow::Result<IngestReport> {
    let mut report = IngestReport::empty();
    let projects_root = claude_projects_dir(roots);
    let project_dirs = list_dirs(&projects_root);

    let mut reconcile_inputs: Vec<ReconcileClaudeRelationshipsInput> = Vec::new();

    for project_dir in project_dirs {
        let files = list_jsonl_files(&project_dir);
        for file in files {
            report.scanned_sessions += 1;
            let key = file.to_string_lossy().into_owned();
            let meta = match fs::metadata(&file) {
                Ok(m) => m,
                Err(err) => {
                    eprintln!("[burn] skipping {}: {}", file.display(), err);
                    continue;
                }
            };

            let prior_claude = match cursors.get_typed(&key) {
                Some(FileCursor::Claude(c)) => Some(c),
                _ => None,
            };
            let inode = file_inode(&meta);
            let mtime = mtime_ms(&meta);
            let size = meta.len();
            let rotated = match &prior_claude {
                None => true,
                Some(c) => c.inode != inode || mtime < c.mtime_ms || size < c.offset_bytes,
            };
            let start_offset = if rotated {
                0
            } else {
                prior_claude.as_ref().map(|c| c.offset_bytes).unwrap_or(0)
            };

            if !rotated && start_offset >= size {
                // Nothing new; refresh mtime bookkeeping and skip parse +
                // reconciliation evidence — `relationshipIdHash` dedup keeps
                // re-emits idempotent.
                if let Some(mut c) = prior_claude.clone() {
                    c.mtime_ms = mtime;
                    cursors.insert(key, FileCursor::Claude(c));
                }
                continue;
            }

            let last_user_text = if rotated {
                None
            } else {
                prior_claude.as_ref().and_then(|c| c.last_user_text.clone())
            };
            let parse_opts = ClaudeParseIncrementalOptions {
                session_path: Some(file.to_string_lossy().into_owned()),
                content_mode: Some(content_mode),
                start_offset: Some(start_offset),
                last_user_text,
                ..Default::default()
            };
            let parsed = match parse_claude_session_incremental(&file, &parse_opts) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("[burn] skipping {}: {}", file.display(), err);
                    continue;
                }
            };

            if !parsed.turns.is_empty() {
                let session_id = parsed.turns[0].session_id.clone();
                let cwd = parsed.turns.first().and_then(|t| t.project.clone());
                let candidate = PendingStampSessionCandidate {
                    harness: PendingStampHarness::Claude,
                    session_id,
                    session_path: Some(file.clone()),
                    session_mtime_ms: Some(mtime),
                    cwd,
                };
                resolve_pending_stamps_for_report(ledger, &candidate, ledger_home, &mut report);

                report.appended_turns += parsed.turns.len();
                report.ingested_sessions += 1;
                ledger.append_turns(&parsed.turns)?;
            }
            if matches!(content_mode, ContentStoreMode::Full) {
                // Claude JSONL files are 1:1 with session_id; derive the id
                // from the first parsed record (mirrors the TS behaviour:
                // `turns[0]?.sessionId ?? content[0]?.sessionId ?? ''`).
                let session_id = parsed
                    .turns
                    .first()
                    .map(|t| t.session_id.as_str())
                    .or_else(|| parsed.content.first().map(|c| c.session_id.as_str()))
                    .unwrap_or("");
                record_session_gap(
                    AdapterName::Claude,
                    session_id,
                    count_new_tool_calls(&parsed.turns),
                    count_new_tool_results(&parsed.content),
                );
            }
            apply_parsed_extras(ledger, &parsed)?;

            reconcile_inputs.push(ReconcileClaudeRelationshipsInput {
                evidence: parsed.evidence,
            });

            let next = ClaudeCursor {
                inode,
                offset_bytes: parsed.end_offset,
                mtime_ms: mtime,
                last_user_text: if parsed.last_user_text.is_empty() {
                    None
                } else {
                    Some(parsed.last_user_text)
                },
            };
            cursors.insert(key, FileCursor::Claude(next));
        }
    }

    if !reconcile_inputs.is_empty() {
        let reconciled = reconcile_claude_session_relationships(&reconcile_inputs);
        if !reconciled.is_empty() {
            ledger.append_relationships(&reconciled)?;
        }
    }
    Ok(report)
}

/// Iterate Codex rollout JSONL files under `~/.codex/sessions/`, drive the
/// resume/cumulative state machine, and call
/// `resolve_pending_stamps_for_session` for fresh ingests so the harness
/// wrapper's pre-spawn manifest gets folded onto the discovered session.
fn ingest_codex_into(
    ledger: &mut Ledger,
    cursors: &mut Cursors,
    roots: &IngestRoots,
    content_mode: ContentStoreMode,
    ledger_home: Option<&Path>,
) -> anyhow::Result<IngestReport> {
    let mut report = IngestReport::empty();
    let sessions_root = codex_sessions_dir(roots);
    for file in walk_jsonl(&sessions_root) {
        report.scanned_sessions += 1;
        let key = file.to_string_lossy().into_owned();
        let meta = match fs::metadata(&file) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                continue;
            }
        };
        let prior_codex = match cursors.get_typed(&key) {
            Some(FileCursor::Codex(c)) => Some(*c),
            _ => None,
        };
        let inode = file_inode(&meta);
        let mtime = mtime_ms(&meta);
        let size = meta.len();
        let rotated = match &prior_codex {
            None => true,
            Some(c) => c.inode != inode || mtime < c.mtime_ms || size < c.offset_bytes,
        };
        let start_offset = if rotated {
            0
        } else {
            prior_codex.as_ref().map(|c| c.offset_bytes).unwrap_or(0)
        };

        if !rotated && start_offset >= size {
            if let Some(mut c) = prior_codex.clone() {
                c.mtime_ms = mtime;
                cursors.insert(key, FileCursor::Codex(Box::new(c)));
            }
            continue;
        }

        let resume = if rotated {
            None
        } else {
            prior_codex.as_ref().map(codex_cursor_to_resume_state)
        };

        let parse_opts = ParseCodexIncrementalOptions {
            session_path: Some(file.to_string_lossy().into_owned()),
            content_mode: Some(content_mode),
            start_offset: Some(start_offset),
            resume,
            ..Default::default()
        };
        let mut parsed = match parse_codex_session_incremental(&file, &parse_opts) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                continue;
            }
        };

        // Take `resume` out so the remaining `parsed` can be borrowed by
        // `apply_parsed_extras` below; the resume state drives only cursor
        // bookkeeping past that point.
        let next_resume = std::mem::take(&mut parsed.resume);
        let mut codex_session_id = if !next_resume.session_id.is_empty() {
            Some(next_resume.session_id.clone())
        } else {
            None
        };
        if codex_session_id.is_none() {
            codex_session_id = parsed
                .turns
                .first()
                .map(|t| t.session_id.clone())
                .filter(|s| !s.is_empty());
        }
        if codex_session_id.is_none() {
            codex_session_id = parsed
                .content
                .first()
                .map(|c| c.session_id.clone())
                .filter(|s| !s.is_empty());
        }
        if codex_session_id.is_none()
            && (!parsed.turns.is_empty()
                || (matches!(content_mode, ContentStoreMode::Full) && !parsed.content.is_empty()))
        {
            codex_session_id = derive_codex_session_id(&file);
        }

        if !parsed.turns.is_empty() {
            if let Some(sid) = &codex_session_id {
                let cwd = next_resume
                    .session_cwd
                    .clone()
                    .or_else(|| parsed.turns.first().and_then(|t| t.project.clone()));
                let candidate = PendingStampSessionCandidate {
                    harness: PendingStampHarness::Codex,
                    session_id: sid.clone(),
                    session_path: Some(file.clone()),
                    session_mtime_ms: Some(mtime),
                    cwd,
                };
                resolve_pending_stamps_for_report(ledger, &candidate, ledger_home, &mut report);
            }
            report.appended_turns += parsed.turns.len();
            report.ingested_sessions += 1;
            ledger.append_turns(&parsed.turns)?;
        }
        if matches!(content_mode, ContentStoreMode::Full) {
            record_session_gap(
                AdapterName::Codex,
                codex_session_id.as_deref().unwrap_or(""),
                count_new_tool_calls(&parsed.turns),
                count_new_tool_results(&parsed.content),
            );
        }
        apply_parsed_extras(ledger, &parsed)?;

        let next = resume_state_to_codex_cursor(&next_resume, inode, parsed.end_offset, mtime);
        cursors.insert(key, FileCursor::Codex(Box::new(next)));
    }
    Ok(report)
}

/// Iterate `ses_*.json` session files under
/// `~/.local/share/opencode/storage/session/`, derive the message-dir
/// mtime, and drive `parse_opencode_session_incremental` with the carried
/// `seenMessageIds`.
fn ingest_opencode_into(
    ledger: &mut Ledger,
    cursors: &mut Cursors,
    roots: &IngestRoots,
    content_mode: ContentStoreMode,
    ledger_home: Option<&Path>,
) -> anyhow::Result<IngestReport> {
    let mut report = IngestReport::empty();
    let session_root = opencode_session_root(roots);
    let message_root = opencode_message_root(roots);
    for file in walk_opencode_sessions(&session_root) {
        report.scanned_sessions += 1;
        let key = file.to_string_lossy().into_owned();
        let session_id = match file.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let message_dir = message_root.join(&session_id);
        let message_mtime = match dir_mtime(&message_dir) {
            Some(t) => t,
            None => continue,
        };
        let meta = match fs::metadata(&file) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                continue;
            }
        };
        let prior = match cursors.get_typed(&key) {
            Some(FileCursor::Opencode(c)) => Some(c),
            _ => None,
        };
        let inode = file_inode(&meta);
        let rotated = match &prior {
            None => true,
            Some(c) => c.inode != inode || message_mtime < c.mtime_ms,
        };
        let seen_message_ids = if rotated {
            std::collections::BTreeSet::new()
        } else {
            prior
                .as_ref()
                .map(|c| c.seen_message_ids.iter().cloned().collect())
                .unwrap_or_default()
        };

        if !rotated {
            if let Some(c) = &prior {
                if message_mtime == c.mtime_ms {
                    continue;
                }
            }
        }

        let parse_opts = ParseOpencodeIncrementalOptions {
            session_path: Some(file.to_string_lossy().into_owned()),
            content_mode: Some(content_mode),
            seen_message_ids: Some(seen_message_ids),
            ..Default::default()
        };
        let parsed = match parse_opencode_session_incremental(&file, &parse_opts) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                continue;
            }
        };

        let session_mtime_ms = mtime_ms(&meta).max(message_mtime);
        if !parsed.turns.is_empty() {
            let cwd = parsed.turns.first().and_then(|t| t.project.clone());
            let candidate = PendingStampSessionCandidate {
                harness: PendingStampHarness::Opencode,
                session_id: session_id.clone(),
                session_path: Some(file.clone()),
                session_mtime_ms: Some(session_mtime_ms),
                cwd,
            };
            resolve_pending_stamps_for_report(ledger, &candidate, ledger_home, &mut report);
            report.appended_turns += parsed.turns.len();
            report.ingested_sessions += 1;
            ledger.append_turns(&parsed.turns)?;
        }
        if matches!(content_mode, ContentStoreMode::Full) {
            record_session_gap(
                AdapterName::Opencode,
                &session_id,
                count_new_tool_calls(&parsed.turns),
                count_new_tool_results(&parsed.content),
            );
        }
        apply_parsed_extras(ledger, &parsed)?;

        let seen: Vec<String> = parsed.seen_message_ids.into_iter().collect();
        let next = OpencodeCursor {
            inode,
            mtime_ms: message_mtime,
            seen_message_ids: seen,
        };
        cursors.insert(key, FileCursor::Opencode(next));
    }
    Ok(report)
}

// --- Codex cursor <-> reader resume-state conversions -------------------

fn codex_cursor_to_resume_state(c: &CodexCursor) -> CodexResumeState {
    let mut turn_contexts: HashMap<String, CodexTurnContext> = HashMap::new();
    for (k, v) in &c.turn_contexts {
        turn_contexts.insert(k.clone(), turn_context_from_value(v));
    }
    let user_turn_slot = c
        .user_turn_slot
        .as_ref()
        .and_then(user_turn_slot_from_value);
    let last_completed_turn = c
        .last_completed_turn
        .as_ref()
        .and_then(last_completed_turn_from_value);
    let mut tool_result_counters: HashMap<String, u64> = HashMap::new();
    if let Some(map) = &c.tool_result_counters {
        for (k, v) in map {
            tool_result_counters.insert(k.clone(), *v);
        }
    }
    CodexResumeState {
        cumulative: ReaderCumulativeUsage {
            input: c.cumulative.input as i64,
            output: c.cumulative.output as i64,
            cache_read: c.cumulative.cache_read as i64,
            reasoning: c.cumulative.reasoning as i64,
        },
        session_id: c.session_id.clone(),
        session_cwd: c.session_cwd.clone(),
        turn_contexts,
        user_turn_slot,
        root_session_emitted: c.root_session_emitted,
        session_meta_relationship_keys: Vec::new(),
        next_event_index: c.next_event_index.unwrap_or(0),
        tool_result_counters,
        last_completed_turn,
    }
}

fn resume_state_to_codex_cursor(
    r: &CodexResumeState,
    inode: u64,
    offset_bytes: u64,
    mtime_ms: i64,
) -> CodexCursor {
    let mut turn_contexts: std::collections::BTreeMap<String, Value> =
        std::collections::BTreeMap::new();
    for (k, v) in &r.turn_contexts {
        turn_contexts.insert(k.clone(), turn_context_to_value(v));
    }
    let tool_result_counters = if r.tool_result_counters.is_empty() {
        None
    } else {
        let mut m: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for (k, v) in &r.tool_result_counters {
            m.insert(k.clone(), *v);
        }
        Some(m)
    };
    CodexCursor {
        inode,
        offset_bytes,
        mtime_ms,
        cumulative: CodexCumulative {
            input: r.cumulative.input.max(0) as u64,
            output: r.cumulative.output.max(0) as u64,
            cache_read: r.cumulative.cache_read.max(0) as u64,
            reasoning: r.cumulative.reasoning.max(0) as u64,
        },
        session_id: r.session_id.clone(),
        session_cwd: r.session_cwd.clone(),
        turn_contexts,
        user_turn_slot: r.user_turn_slot.as_ref().map(user_turn_slot_to_value),
        root_session_emitted: r.root_session_emitted,
        next_event_index: Some(r.next_event_index),
        tool_result_counters,
        last_completed_turn: r
            .last_completed_turn
            .as_ref()
            .map(last_completed_turn_to_value),
    }
}

fn turn_context_from_value(v: &Value) -> CodexTurnContext {
    let obj = v.as_object();
    CodexTurnContext {
        turn_id: obj
            .and_then(|m| m.get("turnId"))
            .and_then(Value::as_str)
            .map(String::from),
        cwd: obj
            .and_then(|m| m.get("cwd"))
            .and_then(Value::as_str)
            .map(String::from),
        model: obj
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .map(String::from),
    }
}

fn turn_context_to_value(c: &CodexTurnContext) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(s) = &c.turn_id {
        m.insert("turnId".into(), Value::String(s.clone()));
    }
    if let Some(s) = &c.cwd {
        m.insert("cwd".into(), Value::String(s.clone()));
    }
    if let Some(s) = &c.model {
        m.insert("model".into(), Value::String(s.clone()));
    }
    Value::Object(m)
}

fn user_turn_slot_from_value(v: &Value) -> Option<PersistedUserTurnSlot> {
    // PersistedUserTurnSlot doesn't derive Deserialize; build it from the
    // JSON object we wrote out via `user_turn_slot_to_value`.
    let obj = v.as_object()?;
    let blocks_v = obj.get("blocks")?.clone();
    let blocks: Vec<crate::reader::UserTurnBlock> = serde_json::from_value(blocks_v).ok()?;
    let preceding_message_id = obj
        .get("precedingMessageId")
        .and_then(Value::as_str)
        .map(String::from);
    let ts = obj.get("ts").and_then(Value::as_str)?.to_string();
    Some(PersistedUserTurnSlot {
        blocks,
        preceding_message_id,
        ts,
    })
}

fn user_turn_slot_to_value(p: &PersistedUserTurnSlot) -> Value {
    // PersistedUserTurnSlot doesn't derive Serialize; build the JSON shape
    // by hand (matches the TS persisted form).
    let mut m = serde_json::Map::new();
    m.insert(
        "blocks".into(),
        serde_json::to_value(&p.blocks).unwrap_or(Value::Array(vec![])),
    );
    if let Some(s) = &p.preceding_message_id {
        m.insert("precedingMessageId".into(), Value::String(s.clone()));
    }
    m.insert("ts".into(), Value::String(p.ts.clone()));
    Value::Object(m)
}

fn last_completed_turn_from_value(v: &Value) -> Option<CodexLastCompletedTurn> {
    let obj = v.as_object()?;
    let message_id = obj.get("messageId")?.as_str()?.to_string();
    let cache_read = obj.get("cacheRead")?.as_u64()?;
    Some(CodexLastCompletedTurn {
        message_id,
        cache_read,
    })
}

fn last_completed_turn_to_value(t: &CodexLastCompletedTurn) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("messageId".into(), Value::String(t.message_id.clone()));
    m.insert(
        "cacheRead".into(),
        Value::Number(serde_json::Number::from(t.cache_read)),
    );
    Value::Object(m)
}

/// Slice accessors shared by every parser result so [`apply_parsed_extras`]
/// can append the trailing derived-record buckets without caring which
/// harness produced them. Turns are deliberately omitted — the harness
/// orchestrators count them, look up the session id, and resolve pending
/// stamps before appending.
trait DerivedRecords {
    fn content(&self) -> &[ContentRecord];
    fn events(&self) -> &[CompactionEvent];
    fn relationships(&self) -> &[SessionRelationshipRecord];
    fn tool_result_events(&self) -> &[ToolResultEventRecord];
    fn user_turns(&self) -> &[UserTurnRecord];
}

macro_rules! impl_derived_records {
    ($ty:ty) => {
        impl DerivedRecords for $ty {
            fn content(&self) -> &[ContentRecord] {
                &self.content
            }
            fn events(&self) -> &[CompactionEvent] {
                &self.events
            }
            fn relationships(&self) -> &[SessionRelationshipRecord] {
                &self.relationships
            }
            fn tool_result_events(&self) -> &[ToolResultEventRecord] {
                &self.tool_result_events
            }
            fn user_turns(&self) -> &[UserTurnRecord] {
                &self.user_turns
            }
        }
    };
}

impl_derived_records!(ClaudeParseResult);
impl_derived_records!(ClaudeParseIncrementalResult);
impl_derived_records!(ParseCodexIncrementalResult);
impl_derived_records!(ParseOpencodeIncrementalResult);

/// Append the trailing derived-record buckets shared by every parser
/// result: content, compactions, relationships, tool-result events, and
/// user-turn rows. Each bucket is gated on non-empty to avoid a no-op
/// transaction.
fn apply_parsed_extras<P: DerivedRecords>(ledger: &mut Ledger, p: &P) -> anyhow::Result<()> {
    if !p.content().is_empty() {
        ledger.append_content(p.content())?;
    }
    if !p.events().is_empty() {
        ledger.append_compactions(p.events())?;
    }
    if !p.relationships().is_empty() {
        ledger.append_relationships(p.relationships())?;
    }
    if !p.tool_result_events().is_empty() {
        ledger.append_tool_result_events(p.tool_result_events())?;
    }
    if !p.user_turns().is_empty() {
        ledger.append_user_turns(p.user_turns())?;
    }
    Ok(())
}

fn resolve_pending_stamps_for_report(
    ledger: &mut Ledger,
    candidate: &PendingStampSessionCandidate,
    ledger_home: Option<&Path>,
    report: &mut IngestReport,
) {
    match resolve_pending_stamps_for_session_in(ledger, candidate, ledger_home) {
        Ok(resolved) => {
            report.applied_pending_stamps += resolved.applied;
        }
        Err(err) => {
            let home = ledger_home
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<default>".to_string());
            eprintln!("[burn] pending stamp resolution failed for {candidate:?} in {home}: {err}");
        }
    }
}

// --- filesystem helpers --------------------------------------------------

fn dir_mtime(dir: &Path) -> Option<i64> {
    let meta = fs::metadata(dir).ok()?;
    Some(mtime_ms(&meta))
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn file_inode(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn file_inode(meta: &fs::Metadata) -> u64 {
    // Best-effort on non-Unix: use file length as a weak rotation signal.
    // We never run there in practice; the binary ships Unix-first.
    meta.len()
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
            applied_pending_stamps: 4,
        };
        let b = IngestReport {
            scanned_sessions: 10,
            ingested_sessions: 20,
            appended_turns: 30,
            applied_pending_stamps: 40,
        };
        a.merge(&b);
        assert_eq!(a.scanned_sessions, 11);
        assert_eq!(a.ingested_sessions, 22);
        assert_eq!(a.appended_turns, 33);
        assert_eq!(a.applied_pending_stamps, 44);
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
