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
    SessionRelationshipRecord, ToolResultEventRecord, TurnRecord, UserTurnRecord,
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
use crate::util::home_dir;

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
    /// Bypass the no-op fast path and always run a full cursor-based sweep.
    /// The watch loop sets this for FS-event-driven ticks: a real `notify`
    /// event can fire while a write is still in flight (file opened, bytes
    /// not yet flushed), so `fs::metadata` — and therefore the stat-only
    /// [`source_fingerprint`] — can still read pre-write size/mtime and
    /// collide with the stored value. Forcing the sweep on an FS event lets
    /// the per-file cursors observe whatever has flushed instead of trusting
    /// the aggregate. The fingerprint is still recorded afterwards, so the
    /// slow polling backstop (which leaves this `false`) keeps the fast path.
    pub force_scan: bool,
}

/// Per-harness root paths. `None` means "use the OS default for this harness".
/// Tests inject explicit roots so they don't scan the developer's home dir.
#[derive(Debug, Clone, Default)]
pub struct IngestRoots {
    pub claude_projects_dir: Option<PathBuf>,
    pub codex_sessions_dir: Option<PathBuf>,
    pub opencode_storage_dir: Option<PathBuf>,
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

const FINGERPRINT_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const FINGERPRINT_HASH_PRIME: u64 = 0x100000001b3;

/// Cheap aggregate over the source session files `ingest_all` scans:
/// `"count:totalBytes:hash"`. The hash folds in each path, byte length,
/// and mtime, so a file added, removed, appended, truncated, touched, or
/// renamed changes the fingerprint in normal operation.
///
/// Computed from `fs::metadata` only — no cursor deserialization and no
/// parse — so it is far cheaper than a full sweep while still detecting
/// in-place appends to existing JSONL files (which directory mtimes miss).
///
/// For OpenCode, appends land as *new files* in `message/<session>/` rather
/// than as growth of the session json, so we fold in both the message-dir
/// mtime (the signal [`ingest_opencode_into`] keys on) AND the dir's child
/// count. The count is the load-bearing one: on filesystems with coarse
/// mtime granularity a new message file can land inside the same tick as the
/// recorded fingerprint, leaving the dir mtime unmoved — but adding the file
/// always bumps the entry count, so the gate still re-opens. (Without the
/// count, the fast path would make that blind spot sticky: it would block the
/// whole sweep until some unrelated change moved the fingerprint.)
///
/// This is a change *detector*, not a content hash: two distinct source
/// states could in principle collide. For append-only session logs within a
/// single inter-sweep window that is not a practical concern, and the worst
/// case is one skipped no-op sweep, never lost data — the per-file cursors
/// still catch up on the next fingerprint change.
fn source_fingerprint(roots: &IngestRoots) -> String {
    let mut count: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut hash_sum: u64 = 0;

    // Claude: ~/.claude/projects/<project>/<session>.jsonl
    let projects_root = claude_projects_dir(roots);
    for project_dir in list_dirs(&projects_root) {
        for file in list_jsonl_files(&project_dir) {
            if let Ok(meta) = fs::metadata(&file) {
                count = count.wrapping_add(1);
                total_bytes = total_bytes.wrapping_add(meta.len());
                hash_sum = hash_sum.wrapping_add(fingerprint_entry_hash("claude", &file, &meta));
            }
        }
    }

    // Codex: ~/.codex/sessions/**/*.jsonl
    for file in walk_jsonl(codex_sessions_dir(roots)) {
        if let Ok(meta) = fs::metadata(&file) {
            count = count.wrapping_add(1);
            total_bytes = total_bytes.wrapping_add(meta.len());
            hash_sum = hash_sum.wrapping_add(fingerprint_entry_hash("codex", &file, &meta));
        }
    }

    // OpenCode: ses_*.json plus, per session, the message-dir mtime and its
    // child count. The count closes the coarse-mtime new-file window (see the
    // doc comment); the adapter watches the same dir for new message files.
    let message_root = opencode_message_root(roots);
    for file in walk_opencode_sessions(opencode_session_root(roots)) {
        if let Ok(meta) = fs::metadata(&file) {
            count = count.wrapping_add(1);
            total_bytes = total_bytes.wrapping_add(meta.len());
            hash_sum = hash_sum.wrapping_add(fingerprint_entry_hash("opencode", &file, &meta));
        }
        if let Some(session_id) = file.file_stem().and_then(|s| s.to_str()) {
            let message_dir = message_root.join(session_id);
            if let Some(t) = dir_mtime(&message_dir) {
                hash_sum = hash_sum.wrapping_add(fingerprint_virtual_hash(
                    "opencode-message",
                    &message_dir,
                    t,
                ));
                count = count.wrapping_add(dir_entry_count(&message_dir));
            }
        }
    }

    format!("{count}:{total_bytes}:{hash_sum:016x}")
}

fn fingerprint_entry_hash(kind: &str, path: &Path, meta: &fs::Metadata) -> u64 {
    fingerprint_hash(kind, path, meta.len(), mtime_ms(meta))
}

fn fingerprint_virtual_hash(kind: &str, path: &Path, mtime_ms: i64) -> u64 {
    fingerprint_hash(kind, path, 0, mtime_ms)
}

fn fingerprint_hash(kind: &str, path: &Path, len: u64, mtime_ms: i64) -> u64 {
    let mut hash = FINGERPRINT_HASH_OFFSET;
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *hash ^= u64::from(*b);
            *hash = hash.wrapping_mul(FINGERPRINT_HASH_PRIME);
        }
    }
    feed(&mut hash, kind.as_bytes());
    feed(&mut hash, b"\0");
    feed(&mut hash, path.to_string_lossy().as_bytes());
    feed(&mut hash, b"\0");
    feed(&mut hash, &len.to_le_bytes());
    feed(&mut hash, &mtime_ms.to_le_bytes());
    hash
}

/// Ingest every known session store once. Cleans stale pending stamps,
/// loads cursors, walks Claude/Codex/OpenCode in turn, then persists any
/// cursor mutations. Returns the merged report.
///
/// Fast path: a stat-only [`source_fingerprint`] is compared against the
/// value the last sweep recorded. When they match, nothing upstream has
/// moved and the call returns an empty report without loading cursors or
/// touching any session file beyond the fingerprint walk. See #468.
pub fn ingest_all(ledger: &mut Ledger, opts: &IngestOptions) -> anyhow::Result<IngestReport> {
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;

    progress(opts, "checking for source changes");
    let current_fp = source_fingerprint(&opts.roots);
    // `force_scan` (set by the watch loop on an FS-event tick) skips the gate:
    // a `notify` event can land before the write is flushed, so the stat-only
    // fingerprint may still match the stored value even though new bytes are
    // coming. A forced full sweep lets the per-file cursors catch up; an
    // unforced tick (slow polling backstop, manual ingest) keeps the fast path.
    if !opts.force_scan {
        match ledger.read_source_fingerprint() {
            Ok(stored) if !stored.is_empty() && stored == current_fp => {
                // Nothing upstream changed since the last sweep — skip the
                // cursor load + per-file deserialize entirely.
                return Ok(IngestReport::empty());
            }
            Ok(_) => {}
            // A read failure shouldn't wedge ingest; fall through to a full sweep.
            Err(_) => {}
        }
    }

    progress(opts, "loading ingest cursors");
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let mut report = IngestReport::empty();

    progress(opts, "loading content settings");
    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    let on_warn: Option<&dyn Fn(&str)> = opts.on_warn.as_ref().map(|f| f.as_ref() as &dyn Fn(&str));

    // Set by any per-harness loop that statted a source file but couldn't read
    // or parse it. Such a file was already folded into `current_fp`, so caching
    // the fingerprint would let a transient failure (e.g. a `PermissionDenied`
    // read later fixed with `chmod`, which moves ctime but not size/mtime) stay
    // sticky: the next sweep would match the stored fingerprint and short-
    // circuit before retrying. When set, we leave the stored fingerprint
    // untouched so the next ingest re-walks and retries the skipped source.
    let mut had_skips = false;

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
        &mut had_skips,
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
        &mut had_skips,
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
        &mut had_skips,
    )?;
    report.merge(&r);
    emit_gap_warning(AdapterName::Opencode, content_mode, on_warn);

    progress(opts, "saving ingest cursors");
    save_cursors_if_changed(ledger, &before, &after).map_err(|e| anyhow::anyhow!(e))?;
    // Record the source fingerprint captured at the start of this sweep so
    // the next call can short-circuit. Storing the *start* value is
    // conservative: any file that changed mid-sweep yields a different
    // fingerprint next time, forcing a re-scan (cheap, since per-file
    // cursors then skip the already-read bytes).
    //
    // Ordering is deliberate: cursors are saved first, fingerprint last and
    // in a separate statement. A crash between them leaves cursors advanced
    // but the fingerprint stale/blank, which only forces one extra full scan
    // (cursors then find nothing new) — never a missed append. We must never
    // store a fingerprint *ahead* of the cursors, so the fingerprint write
    // stays after the cursor save.
    //
    // Skip the write entirely when a source file was counted into `current_fp`
    // but couldn't be read/parsed: caching would make that skip sticky (see
    // `had_skips` above). Leaving the stored fingerprint stale costs at most a
    // full re-scan next time — a persistently-broken file keeps the fast path
    // off until it parses, which is the conservative trade (never a lost append
    // over a perf win).
    if !had_skips {
        ledger
            .write_source_fingerprint(&current_fp)
            .map_err(|e| anyhow::anyhow!(e))?;
    }
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
        &mut bool,
    ) -> anyhow::Result<IngestReport>,
{
    progress(opts, "cleaning pending spawn stamps");
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;
    let before = load_cursors(ledger).map_err(|e| anyhow::anyhow!(e))?;
    let mut after = before.clone();
    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    // The single-harness verbs don't gate on the source fingerprint, so the
    // skip flag is only consumed by `ingest_all`; capture and ignore it here.
    let mut had_skips = false;
    let report = body(
        ledger,
        &mut after,
        &opts.roots,
        content_mode,
        opts.ledger_home.as_deref(),
        &mut had_skips,
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
    ingest_claude_transcript_path(ledger, &file, opts)
}

/// Path-based Claude fast-path used by hook callers that already know the
/// transcript path (e.g. the Claude Code `Stop` hook payload). Parses the
/// single JSONL file, appends turns/content/events/relationships/etc.,
/// resolves any pending stamps for the session, then persists an EOF
/// cursor so a follow-up `ingest_all` skips it. Missing files or
/// non-files return an empty report silently so hook orchestrators
/// don't fail the parent invocation — the caller decides whether to
/// surface the rotation/missing condition.
pub fn ingest_claude_transcript_path(
    ledger: &mut Ledger,
    transcript_path: &Path,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    let file = transcript_path;
    match fs::metadata(file) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Ok(IngestReport::empty()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Hook policy: never break the parent when the transcript
            // file has rotated or never landed. Stay silent so the
            // caller decides whether to surface this. Permission / IO
            // errors fall through below so the caller sees them.
            return Ok(IngestReport::empty());
        }
        Err(err) => return Err(err.into()),
    }

    let content_mode = resolve_content_mode(opts.ledger_home.as_deref());
    let parse_opts = ClaudeParseOptions {
        session_path: Some(file.to_string_lossy().into_owned()),
        content_mode: Some(content_mode),
        ..Default::default()
    };
    let result = parse_claude_session(file, &parse_opts).map_err(|e| anyhow::anyhow!(e))?;
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

    // Resolve pending stamps for the session before marking the file
    // fully ingested — same shape as the per-harness sweep at
    // `ingest_claude_into`, just scoped to the one session this
    // fast-path handles.
    let mut report = IngestReport {
        scanned_sessions: 1,
        ingested_sessions: 1,
        appended_turns,
        applied_pending_stamps: 0,
    };
    let pre_cursor_meta = fs::metadata(file)?;
    let session_id = result.turns[0].session_id.clone();
    let cwd = result.turns.first().and_then(|t| t.project.clone());
    let candidate = PendingStampSessionCandidate {
        harness: PendingStampHarness::Claude,
        session_id,
        session_path: Some(file.to_path_buf()),
        session_mtime_ms: Some(mtime_ms(&pre_cursor_meta)),
        cwd,
    };
    resolve_pending_stamps_for_report(ledger, &candidate, opts.ledger_home.as_deref(), &mut report);

    // Re-stat after parsing so the cursor reflects the byte position the
    // parser actually read to. `parse_claude_session` uses BufReader::lines()
    // and keeps reading past the pre-parse `len()` if the file grew during
    // parse; using the pre-parse size would cause a follow-up `ingest_all`
    // to replay those bytes and emit duplicate turns.
    let meta = fs::metadata(file)?;
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

    Ok(report)
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
    had_skips: &mut bool,
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
                    *had_skips = true;
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
                    *had_skips = true;
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
    had_skips: &mut bool,
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
                *had_skips = true;
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
                *had_skips = true;
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
    had_skips: &mut bool,
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
                *had_skips = true;
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
                *had_skips = true;
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
    /// Per-turn upstream-`requestId` lookup, keyed by `(source,
    /// session_id, message_id)`. Default returns an empty cow; harnesses
    /// without a `requestId` equivalent (Codex, opencode) accept this
    /// default and the inference builder falls back to `message_id`. See
    /// issue #434 and `reader::inference`.
    fn request_id_lookup(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
        std::borrow::Cow::Owned(crate::reader::RequestIdLookup::new())
    }
    /// Turns the parser produced. The trailing-record appenders don't
    /// strictly need it (the harness orchestrators count + append turns
    /// themselves before calling [`apply_parsed_extras`]); the
    /// `apply_parsed_extras` inference materializer uses it to rebuild
    /// the per-API-call rows in lockstep with the persisted turns.
    fn turns(&self) -> &[TurnRecord];
}

/// Shared accessor body for the four parser-result types. The
/// `request_id_lookup` override is intentionally NOT in this macro —
/// Claude's two result types implement it directly below, while Codex /
/// opencode inherit the trait default empty `RequestIdLookup`. Splitting
/// the override out keeps the macro single-arm and avoids macro-level
/// boolean branching that `macro_rules!` doesn't natively support.
macro_rules! impl_derived_records_common {
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
            fn turns(&self) -> &[TurnRecord] {
                &self.turns
            }
            fn request_id_lookup(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
                // Default: empty lookup (Codex / opencode). Claude
                // overrides this in the per-impl block below; we can't
                // express that override via a single macro arm because
                // `macro_rules!` literals don't dispatch.
                claude_request_id_lookup_for(self)
            }
        }
    };
}

impl_derived_records_common!(ClaudeParseResult);
impl_derived_records_common!(ClaudeParseIncrementalResult);
impl_derived_records_common!(ParseCodexIncrementalResult);
impl_derived_records_common!(ParseOpencodeIncrementalResult);

/// Per-type adapter for the `request_id_lookup` override (issue #434).
/// Specialized for Claude's two result types so they borrow the parser's
/// real lookup; the generic fallback returns an empty owned lookup so
/// Codex / opencode behave as "no requestId".
trait ClaudeRequestIdSource {
    fn lookup_cow(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup>;
}

impl ClaudeRequestIdSource for ClaudeParseResult {
    fn lookup_cow(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
        std::borrow::Cow::Borrowed(&self.request_id_lookup)
    }
}
impl ClaudeRequestIdSource for ClaudeParseIncrementalResult {
    fn lookup_cow(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
        std::borrow::Cow::Borrowed(&self.request_id_lookup)
    }
}
impl ClaudeRequestIdSource for ParseCodexIncrementalResult {
    fn lookup_cow(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
        std::borrow::Cow::Owned(crate::reader::RequestIdLookup::new())
    }
}
impl ClaudeRequestIdSource for ParseOpencodeIncrementalResult {
    fn lookup_cow(&self) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
        std::borrow::Cow::Owned(crate::reader::RequestIdLookup::new())
    }
}

fn claude_request_id_lookup_for<P: ClaudeRequestIdSource + ?Sized>(
    p: &P,
) -> std::borrow::Cow<'_, crate::reader::RequestIdLookup> {
    p.lookup_cow()
}

/// Append the trailing derived-record buckets shared by every parser
/// result: content, compactions, relationships, tool-result events,
/// user-turn rows, and (issue #434) per-API-call inferences. Each
/// bucket is gated on non-empty to avoid a no-op transaction.
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
    // Materialize inferences from the same turn slice the orchestrator
    // appended. Building here (not in the parser) keeps the inference
    // table in lockstep with what actually got persisted — if a turn
    // was deduped at append time by the content-fingerprint check, its
    // inference will simply re-replace the prior row via the
    // `INSERT OR REPLACE` writer, which is the correct steady-state.
    if !p.turns().is_empty() {
        let lookup = p.request_id_lookup();
        let inferences = crate::reader::build_inferences(p.turns(), lookup.as_ref());
        if !inferences.is_empty() {
            ledger.append_inferences(&inferences)?;
        }
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

/// Number of direct entries in `dir`, or `0` if it can't be read. Used by the
/// source fingerprint to detect a new OpenCode message file even when the
/// directory mtime granularity is too coarse to register the add within the
/// same tick.
fn dir_entry_count(dir: &Path) -> u64 {
    match fs::read_dir(dir) {
        Ok(entries) => entries.filter(|e| e.is_ok()).count() as u64,
        Err(_) => 0,
    }
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

    #[test]
    fn source_fingerprint_is_stable_and_moves_on_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let roots = IngestRoots {
            claude_projects_dir: Some(tmp.path().join("claude")),
            codex_sessions_dir: Some(tmp.path().join("codex")),
            opencode_storage_dir: Some(tmp.path().join("opencode")),
        };
        // Empty roots: well-formed, stable, and identical across calls.
        let empty = source_fingerprint(&roots);
        assert_eq!(empty, source_fingerprint(&roots));
        assert_eq!(empty, "0:0:0000000000000000");

        let project = roots.claude_projects_dir.as_ref().unwrap().join("proj");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("s1.jsonl");
        fs::write(&file, "a\n").unwrap();
        let with_file = source_fingerprint(&roots);
        assert_ne!(with_file, empty, "adding a file must move the fingerprint");

        // Growing the file (size + mtime) must move it again.
        fs::write(&file, "a\nbb\n").unwrap();
        let appended = source_fingerprint(&roots);
        assert_ne!(
            appended, with_file,
            "appending bytes must move the fingerprint"
        );

        // Deleting the file (count drops) must move it back toward empty.
        fs::remove_file(&file).unwrap();
        let deleted = source_fingerprint(&roots);
        assert_ne!(
            deleted, appended,
            "deleting a file must move the fingerprint"
        );
        assert_eq!(deleted, empty, "back to zero files == empty fingerprint");
    }

    #[test]
    fn source_fingerprint_moves_on_new_opencode_message_file() {
        // M1 regression: an OpenCode append lands as a NEW file under
        // message/<session>/, not as growth of the ses_*.json. Folding the
        // message-dir child count into the fingerprint guarantees the gate
        // re-opens even if the dir mtime granularity is too coarse to move.
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = tmp.path().join("opencode");
        let roots = IngestRoots {
            claude_projects_dir: Some(tmp.path().join("claude")),
            codex_sessions_dir: Some(tmp.path().join("codex")),
            opencode_storage_dir: Some(storage.clone()),
        };

        let session_dir = storage.join("session");
        fs::create_dir_all(&session_dir).unwrap();
        let session_file = session_dir.join("ses_abc.json");
        fs::write(&session_file, "{}").unwrap();

        let message_dir = storage.join("message").join("ses_abc");
        fs::create_dir_all(&message_dir).unwrap();
        fs::write(message_dir.join("msg_1.json"), "{}").unwrap();
        let before = source_fingerprint(&roots);

        // Add a second message file WITHOUT touching ses_abc.json. The session
        // file size/mtime are unchanged; only the message-dir child count moves.
        fs::write(message_dir.join("msg_2.json"), "{}").unwrap();
        let after = source_fingerprint(&roots);
        assert_ne!(
            after, before,
            "a new message file must move the fingerprint via the dir child count"
        );
    }
}
