//! GitHub Copilot CLI ingest: OTEL export discovery, cursor-driven
//! incremental parsing, and ledger persistence.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    apply_parsed_extras, file_inode, fingerprint_entry_hash, mtime_ms, run_single_harness,
    DerivedRecords, IngestOptions, IngestReport, IngestRoots,
};
use crate::ingest::cursors::{CopilotCursor, Cursors, FileCursor};
use crate::ingest::gap::AdapterName;
use crate::ingest::walk::list_jsonl_files;
use crate::ledger::Ledger;
use crate::reader::{
    parse_copilot_otel_incremental, CompactionEvent, ContentRecord, ContentStoreMode,
    CopilotResumeState, ParseCopilotIncrementalOptions, ParseCopilotIncrementalResult,
    SessionRelationshipRecord, SourceKind, ToolResultEventRecord, TurnRecord, UserTurnRecord,
};
use crate::util::home_dir;

/// Default OTEL export directory Copilot CLI writes when the exporter is
/// pointed at a directory: `$COPILOT_HOME/otel`, falling back to
/// `~/.copilot/otel`.
fn copilot_otel_dir() -> PathBuf {
    std::env::var("COPILOT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".copilot"))
        .join("otel")
}

/// Resolve the Copilot CLI OTEL JSONL files ingest scans. Unlike the other
/// harnesses this source is env-gated: nothing is scanned until the user
/// sets `COPILOT_OTEL_FILE_EXPORTER_PATH` (see #14), so an empty list is
/// the normal steady state and never an error. An explicit
/// [`IngestRoots::copilot_otel_files`] override bypasses the gate so tests
/// can inject files directly.
pub(super) fn copilot_otel_files(roots: &IngestRoots) -> Vec<PathBuf> {
    if let Some(files) = &roots.copilot_otel_files {
        return files.clone();
    }
    let configured = std::env::var("COPILOT_OTEL_FILE_EXPORTER_PATH")
        .map(|var| var.trim().to_owned())
        .unwrap_or_default();
    if configured.is_empty() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let p = PathBuf::from(&configured);
    if p.is_file() {
        files.push(p);
    }
    for file in list_jsonl_files(&copilot_otel_dir()) {
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

/// Fold the Copilot OTEL export files into the ingest source fingerprint.
/// Env-gated, so normally empty — but when set, appends must move the
/// fingerprint like any other source.
pub(super) fn fingerprint_exports(
    roots: &IngestRoots,
    count: &mut u64,
    total_bytes: &mut u64,
    hash_sum: &mut u64,
) {
    for file in copilot_otel_files(roots) {
        if let Ok(meta) = fs::metadata(&file) {
            *count = count.wrapping_add(1);
            *total_bytes = total_bytes.wrapping_add(meta.len());
            *hash_sum = hash_sum.wrapping_add(fingerprint_entry_hash("copilot", &file, &meta));
        }
    }
}

pub fn ingest_copilot_sessions(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    run_single_harness(ledger, opts, AdapterName::Copilot, ingest_copilot_into)
}

/// Copilot watch roots: the injected files' parents in tests / overrides,
/// the default otel dir otherwise (the env-var file's appends are also
/// caught by the polling fingerprint even without an FS event).
pub(super) fn push_watch_dirs(roots: &IngestRoots, dirs: &mut Vec<PathBuf>) {
    match &roots.copilot_otel_files {
        Some(files) => {
            for parent in files.iter().filter_map(|f| f.parent()) {
                if !dirs.iter().any(|d| d == parent) {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
        None => dirs.push(copilot_otel_dir()),
    }
}

/// Iterate the Copilot CLI OTEL export files (`COPILOT_OTEL_FILE_EXPORTER_PATH`
/// plus `$COPILOT_HOME/otel/*.jsonl`), driving
/// [`parse_copilot_otel_incremental`] with the carried per-session turn
/// counters and chat-trace set. Env-gated: without
/// `COPILOT_OTEL_FILE_EXPORTER_PATH` (and without an explicit roots
/// override) the file list is empty and this is a silent no-op (#14).
///
/// Rotation handling matches the Claude/Codex adapters: an inode change or
/// a shrunken file restarts the byte offset at 0, but the per-session
/// `turn_index` counters and `chat_trace_ids` survive — the exporter's new
/// file continues the same Copilot sessions. Spans re-read at the rotation
/// boundary that the ledger already holds are skipped before they consume
/// a `turn_index`.
pub(super) fn ingest_copilot_into(
    ledger: &mut Ledger,
    cursors: &mut Cursors,
    roots: &IngestRoots,
    _content_mode: ContentStoreMode,
    _ledger_home: Option<&Path>,
    had_skips: &mut bool,
) -> anyhow::Result<IngestReport> {
    let mut report = IngestReport::empty();
    for file in copilot_otel_files(roots) {
        report.scanned_sessions += 1;
        match fs::metadata(&file) {
            Ok(meta) => ingest_copilot_file(ledger, cursors, &file, &meta, &mut report, had_skips)?,
            // The env var can point at a file the exporter has not created
            // yet; that's the pre-first-session steady state, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                *had_skips = true;
            }
        }
    }
    Ok(report)
}

/// Byte offset to resume `prior` from, or `None` when the file rotated (new
/// inode, regressed mtime, or shrank) and must be re-read from byte 0.
fn resume_offset(prior: &CopilotCursor, inode: u64, mtime: i64, size: u64) -> Option<u64> {
    let rotated = prior.inode != inode || mtime < prior.mtime_ms || size < prior.offset_bytes;
    (!rotated).then_some(prior.offset_bytes)
}

fn ingest_copilot_file(
    ledger: &mut Ledger,
    cursors: &mut Cursors,
    file: &Path,
    meta: &fs::Metadata,
    report: &mut IngestReport,
    had_skips: &mut bool,
) -> anyhow::Result<()> {
    let key = file.to_string_lossy().into_owned();
    let prior = match cursors.get_typed(&key) {
        Some(FileCursor::Copilot(c)) => Some(c),
        _ => None,
    };
    let (inode, mtime, size) = (file_inode(meta), mtime_ms(meta), meta.len());
    let offset = prior
        .as_ref()
        .and_then(|c| resume_offset(c, inode, mtime, size));
    if let (Some(start), Some(mut c)) = (offset, prior.clone()) {
        if start >= size {
            c.mtime_ms = mtime;
            cursors.insert(key, FileCursor::Copilot(c));
            return Ok(());
        }
    }

    // A rotation restarts at byte 0 and can replay spans the ledger
    // already holds; the parser skips those before they take a
    // `turn_index`.
    let persisted = if prior.is_some() && offset.is_none() {
        ledger.turn_keys_for_source(SourceKind::CopilotCli)?
    } else {
        HashSet::new()
    };
    let parse_opts = ParseCopilotIncrementalOptions {
        session_path: Some(key.clone()),
        start_offset: Some(offset.unwrap_or(0)),
        resume: prior.map(|c| CopilotResumeState {
            session_turn_counts: c.session_turn_counts,
            chat_trace_ids: c.chat_trace_ids,
        }),
        fallback_ts_ms: Some(mtime),
        persisted,
    };
    let parsed: ParseCopilotIncrementalResult =
        match parse_copilot_otel_incremental(file, &parse_opts) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("[burn] skipping {}: {}", file.display(), err);
                *had_skips = true;
                return Ok(());
            }
        };

    if !parsed.turns.is_empty() {
        report.appended_turns += parsed.turns.len();
        report.ingested_sessions += 1;
        ledger.append_turns(&parsed.turns)?;
    }
    // Keep the inference table in lockstep with the persisted turns,
    // like every other harness path (issue #434) — otherwise
    // `burn flow` and span-tree reads see no Copilot API calls.
    apply_parsed_extras(ledger, &parsed)?;

    let next = CopilotCursor {
        inode,
        offset_bytes: parsed.end_offset,
        mtime_ms: mtime,
        session_turn_counts: parsed.resume.session_turn_counts,
        chat_trace_ids: parsed.resume.chat_trace_ids,
    };
    cursors.insert(key, FileCursor::Copilot(next));
    Ok(())
}

/// The Copilot OTEL parser emits usage-only turns with no trailing content,
/// compaction, relationship, tool-result, or user-turn buckets, so every
/// bucket but `turns` is empty. The `turns` slice feeds the
/// `apply_parsed_extras` inference materializer; the request-id lookup
/// stays the trait default (empty) since Copilot spans carry no `requestId`
/// equivalent, and the inference builder falls back to `message_id`.
impl DerivedRecords for ParseCopilotIncrementalResult {
    fn content(&self) -> &[ContentRecord] {
        &[]
    }
    fn events(&self) -> &[CompactionEvent] {
        &[]
    }
    fn relationships(&self) -> &[SessionRelationshipRecord] {
        &[]
    }
    fn tool_result_events(&self) -> &[ToolResultEventRecord] {
        &[]
    }
    fn user_turns(&self) -> &[UserTurnRecord] {
        &[]
    }
    fn turns(&self) -> &[TurnRecord] {
        &self.turns
    }
}
