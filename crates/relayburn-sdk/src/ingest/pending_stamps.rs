//! Pending-stamp coordination — Rust port of `packages/ingest/src/pending-stamps.ts`.
//!
//! Launchers that spawn a child process before the session id is known drop a
//! JSON manifest into
//! `$RELAYBURN_HOME/pending-stamps/` (or an explicitly supplied ledger home).
//! After the child exits, the next ingest pass tries to match each manifest
//! against a freshly-discovered session and folds the manifest's enrichment
//! into the ledger via `Ledger::append_stamp`.
//!
//! ## Wire-format compatibility
//!
//! The on-disk JSON shape is stable so external launchers and Rust ingest can
//! coordinate through the same pending-stamp directory. Specifically:
//!
//! * Object keys are emitted in insertion order: `v`, `harness`, `spawnerPid`,
//!   `spawnStartTs`, `cwd`, `enrichment`, then optional `sessionDirHint`.
//! * The file is `JSON.stringify(record, null, 2) + '\n'` — pretty-printed
//!   with two-space indent and a trailing newline, matching `node:fs`.
//! * The filename pattern is `<harness>-<spawnerPid>-<spawnStartMs>-<uuid>.json`
//!   in the selected `pending-stamps/` directory. In-flight writes go through
//!   a `.tmp-<pid>-<uuid>` sibling and are atomically renamed.
//! * Claimed manifests are renamed to `<file>.claimed-<pid>-<uuid>` before the
//!   ledger append, then unlinked on success or restored on failure.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ledger::{ledger_home, Enrichment, Ledger, Stamp, StampSelector};
use serde::{Deserialize, Serialize};

/// 24h — manifests older than this are presumed orphaned (the spawner died
/// before its child wrote a session log) and get garbage-collected on the
/// next ingest pass.
pub const PENDING_STAMP_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Slack the mtime comparison uses when matching a manifest against a
/// candidate session. Filesystem mtimes round to 1ms on macOS APFS, so an
/// exact `>=` would falsely reject same-instant pairs.
const MTIME_SLOP_MS: i64 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PendingStampHarness {
    #[default]
    Codex,
    Claude,
    Opencode,
}

impl PendingStampHarness {
    fn as_str(self) -> &'static str {
        match self {
            PendingStampHarness::Codex => "codex",
            PendingStampHarness::Claude => "claude",
            PendingStampHarness::Opencode => "opencode",
        }
    }
}

/// Parsed manifest. Fields are ordered to match the TS object-literal
/// insertion order so re-serialisation is byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStamp {
    pub v: u8,
    pub harness: PendingStampHarness,
    pub spawner_pid: u32,
    pub spawn_start_ts: String,
    pub cwd: String,
    pub enrichment: Enrichment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_dir_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStampWriteResult {
    pub file: PathBuf,
    pub stamp: PendingStamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStampSessionCandidate {
    pub harness: PendingStampHarness,
    pub session_id: String,
    pub session_path: Option<PathBuf>,
    pub session_mtime_ms: Option<i64>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingStampResolveResult {
    pub applied: usize,
    pub enrichment: Enrichment,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingStampCleanupResult {
    pub scanned: usize,
    pub deleted: usize,
}

#[derive(Debug, Default, Clone)]
pub struct WriteOptions {
    pub harness: PendingStampHarness,
    pub ledger_home: Option<PathBuf>,
    pub cwd: String,
    pub enrichment: Enrichment,
    pub session_dir_hint: Option<String>,
    pub spawn_start_ts: Option<SystemTime>,
    pub spawner_pid: Option<u32>,
}

/// Default location: `$RELAYBURN_HOME/pending-stamps/`.
pub fn pending_stamps_dir() -> PathBuf {
    ledger_home().join("pending-stamps")
}

/// Pending-stamp directory under an explicit ledger home.
pub fn pending_stamps_dir_at_home(home: &Path) -> PathBuf {
    home.join("pending-stamps")
}

fn pending_stamps_dir_for(home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) => pending_stamps_dir_at_home(home),
        None => pending_stamps_dir(),
    }
}

/// Write a manifest for a freshly-spawned harness child. Cleanup runs first
/// so old stamps don't leak into the matcher.
pub fn write_pending_stamp(opts: WriteOptions) -> std::io::Result<PendingStampWriteResult> {
    let spawn_start = opts.spawn_start_ts.unwrap_or_else(SystemTime::now);
    cleanup_stale_pending_stamps_in_at(
        opts.ledger_home.as_deref(),
        spawn_start,
        PENDING_STAMP_TTL_MS,
    )?;

    let stamp = PendingStamp {
        v: 1,
        harness: opts.harness,
        spawner_pid: opts.spawner_pid.unwrap_or_else(std::process::id),
        spawn_start_ts: format_iso_8601(spawn_start),
        cwd: canonicalize_lossy(Path::new(&opts.cwd)),
        enrichment: opts.enrichment,
        session_dir_hint: opts
            .session_dir_hint
            .as_deref()
            .map(|p| canonicalize_lossy(Path::new(p))),
    };

    let dir = pending_stamps_dir_for(opts.ledger_home.as_deref());
    fs::create_dir_all(&dir)?;
    let spawn_ms = system_time_ms(spawn_start);
    let uuid = Uuid::new_v4();
    let base = format!(
        "{}-{}-{}-{}",
        stamp.harness.as_str(),
        stamp.spawner_pid,
        spawn_ms,
        uuid
    );
    let final_path = dir.join(format!("{base}.json"));
    let tmp_path = dir.join(format!(
        "{base}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));

    let payload = serialize_stamp(&stamp);
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(payload.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, &final_path)?;

    Ok(PendingStampWriteResult {
        file: final_path,
        stamp,
    })
}

/// Drop every manifest older than `ttl_ms` (default `PENDING_STAMP_TTL_MS`).
/// Best-effort: filesystem errors on individual files are swallowed so a
/// jammed permission doesn't block the rest of the cleanup pass.
pub fn cleanup_stale_pending_stamps() -> std::io::Result<PendingStampCleanupResult> {
    cleanup_stale_pending_stamps_at(SystemTime::now(), PENDING_STAMP_TTL_MS)
}

pub fn cleanup_stale_pending_stamps_in(
    ledger_home: Option<&Path>,
) -> std::io::Result<PendingStampCleanupResult> {
    cleanup_stale_pending_stamps_in_at(ledger_home, SystemTime::now(), PENDING_STAMP_TTL_MS)
}

pub fn cleanup_stale_pending_stamps_at(
    now: SystemTime,
    ttl_ms: u64,
) -> std::io::Result<PendingStampCleanupResult> {
    cleanup_stale_pending_stamps_in_at(None, now, ttl_ms)
}

pub fn cleanup_stale_pending_stamps_in_at(
    ledger_home: Option<&Path>,
    now: SystemTime,
    ttl_ms: u64,
) -> std::io::Result<PendingStampCleanupResult> {
    let now_ms = system_time_ms(now);
    let dir = pending_stamps_dir_for(ledger_home);
    let files = list_pending_stamp_files_in(&dir, false)?;
    let scanned = files.len();
    let mut deleted = 0usize;

    for file in files {
        let mut should_delete = false;
        match fs::read_to_string(&file) {
            Ok(raw) => match parse_pending_stamp(&raw) {
                Some(parsed) => {
                    if let Some(spawn_ms) = parse_iso_ms(&parsed.spawn_start_ts) {
                        if now_ms.saturating_sub(spawn_ms) > ttl_ms as i64 {
                            should_delete = true;
                        }
                    }
                }
                None => {
                    if let Ok(meta) = fs::metadata(&file) {
                        let mtime = mtime_ms(&meta);
                        if now_ms.saturating_sub(mtime) > ttl_ms as i64 {
                            should_delete = true;
                        }
                    }
                }
            },
            Err(_) => {
                should_delete = true;
            }
        }

        if should_delete && fs::remove_file(&file).is_ok() {
            deleted += 1;
        }
    }

    Ok(PendingStampCleanupResult { scanned, deleted })
}

/// Try to claim and apply every manifest that matches `candidate`. Sorted by
/// `spawnStartTs` (FIFO) so multiple concurrent same-harness/same-cwd runs
/// don't all collapse onto the first session that ingests.
///
/// Side effects: each successfully claimed manifest's enrichment is folded
/// onto the candidate session via `ledger.append_stamp`. The manifest file is
/// removed on success and restored if the ledger append errors.
pub fn resolve_pending_stamps_for_session_in(
    ledger: &mut Ledger,
    candidate: &PendingStampSessionCandidate,
    ledger_home: Option<&Path>,
) -> std::io::Result<PendingStampResolveResult> {
    if candidate.session_id.is_empty() {
        return Ok(PendingStampResolveResult::default());
    }

    cleanup_stale_pending_stamps_in(ledger_home)?;
    let dir = pending_stamps_dir_for(ledger_home);
    let files = list_pending_stamp_files_in(&dir, true)?;
    let mut matches: Vec<(PathBuf, PendingStamp)> = Vec::new();
    for file in files {
        let raw = match fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(record) = parse_pending_stamp(&raw) else {
            continue;
        };
        if pending_stamp_matches(&record, candidate) {
            matches.push((file, record));
        }
    }
    matches.sort_by(|a, b| a.1.spawn_start_ts.cmp(&b.1.spawn_start_ts));

    let mut enrichment = Enrichment::new();
    let mut applied = 0usize;
    for (file, record) in matches {
        let Some(claimed) = claim_pending_stamp(&file)? else {
            continue;
        };
        let selector = StampSelector {
            session_id: Some(candidate.session_id.clone()),
            ..Default::default()
        };
        let stamp = match Stamp::new(
            format_iso_8601(SystemTime::now()),
            selector,
            record.enrichment.clone(),
        ) {
            Ok(s) => s,
            Err(_) => {
                // Empty enrichment / empty selector: treat as a no-op claim.
                let _ = fs::rename(&claimed, &file);
                continue;
            }
        };
        match ledger.append_stamp(&stamp) {
            Ok(()) => {
                for (k, v) in &record.enrichment {
                    enrichment.insert(k.clone(), v.clone());
                }
                applied += 1;
                let _ = fs::remove_file(&claimed);
                // FIFO: claim at most one stamp per call, matching the TS
                // adapter's `break` after the first successful append.
                break;
            }
            Err(err) => {
                // Restore on failure so a later pass can retry.
                let _ = fs::rename(&claimed, &file);
                return Err(std::io::Error::other(err.to_string()));
            }
        }
    }

    Ok(PendingStampResolveResult {
        applied,
        enrichment,
    })
}

// --- internals ----------------------------------------------------------

fn pending_stamp_matches(record: &PendingStamp, candidate: &PendingStampSessionCandidate) -> bool {
    if record.harness != candidate.harness {
        return false;
    }
    if let (Some(hint), Some(session_path)) = (&record.session_dir_hint, &candidate.session_path) {
        let hint_canon = Path::new(hint);
        let session_canon = canonicalize_lossy_path(session_path);
        if !path_starts_with(&session_canon, hint_canon) {
            return false;
        }
    }

    let Some(spawn_ms) = parse_iso_ms(&record.spawn_start_ts) else {
        return false;
    };
    if let Some(mtime) = candidate.session_mtime_ms {
        if mtime.saturating_add(MTIME_SLOP_MS) < spawn_ms {
            return false;
        }
    }
    if let Some(cwd) = &candidate.cwd {
        return canonicalize_lossy(Path::new(cwd)) == record.cwd;
    }
    // Fallback when reader cannot recover session cwd: rely on mtime causality.
    candidate
        .session_mtime_ms
        .map(|m| m.saturating_add(MTIME_SLOP_MS) >= spawn_ms)
        .unwrap_or(false)
}

fn claim_pending_stamp(file: &Path) -> std::io::Result<Option<PathBuf>> {
    let claimed = with_suffix(
        file,
        &format!(".claimed-{}-{}", std::process::id(), Uuid::new_v4()),
    );
    match fs::rename(file, &claimed) {
        Ok(()) => Ok(Some(claimed)),
        Err(_) => Ok(None),
    }
}

fn list_pending_stamp_files_in(dir: &Path, active_only: bool) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(err) => return Err(err),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if active_only && !name_str.ends_with(".json") {
            continue;
        }
        out.push(dir.join(name_str));
    }
    Ok(out)
}

/// Parse a manifest. Returns `None` for any structural defect — callers
/// treat unparseable manifests as cleanup candidates rather than errors so a
/// single corrupt file can't deadlock the pending-stamp directory.
pub fn parse_pending_stamp(raw: &str) -> Option<PendingStamp> {
    let stamp: PendingStamp = serde_json::from_str(raw).ok()?;
    if stamp.v != 1 || parse_iso_ms(&stamp.spawn_start_ts).is_none() || stamp.cwd.is_empty() {
        return None;
    }
    Some(stamp)
}

/// Serialize a manifest in the exact wire format the TS adapter writes:
/// `JSON.stringify(record, null, 2) + '\n'`.
pub fn serialize_stamp(stamp: &PendingStamp) -> String {
    let mut s = serde_json::to_string_pretty(stamp).expect("serializable");
    s.push('\n');
    s
}

fn format_iso_8601(t: SystemTime) -> String {
    // Mirror JS `toISOString()` — ms-precision UTC: `2024-05-04T12:34:56.789Z`.
    let dur = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0));
    let nanos = dur.as_nanos() as i128;
    let dt = OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    dt.format(&fmt).expect("format ms iso")
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    // Accept the JS `Date.parse` shapes we actually emit:
    // `YYYY-MM-DDTHH:MM:SS[.fff]Z`. RFC3339 covers both — variable
    // subsecond precision plus a `Z` suffix.
    let dt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    Some((dt.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn system_time_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Best-effort canonicalization that matches `path.resolve` semantics: returns
/// an absolute, normalized representation of `p` even if the path doesn't
/// exist on disk. We never want to fail the manifest-match step over a
/// missing directory.
fn canonicalize_lossy(p: &Path) -> String {
    canonicalize_lossy_path(p).to_string_lossy().into_owned()
}

fn canonicalize_lossy_path(p: &Path) -> PathBuf {
    if p.is_absolute() {
        normalize_path(p)
    } else if let Ok(cwd) = std::env::current_dir() {
        normalize_path(&cwd.join(p))
    } else {
        p.to_path_buf()
    }
}

fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in p.components() {
        use std::path::Component::*;
        match component {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_starts_with(child: &Path, ancestor: &Path) -> bool {
    let a = normalize_path(ancestor);
    let c = normalize_path(child);
    c.starts_with(&a)
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_resolve_pending_stamp_can_share_explicit_home() {
        let home = tempfile::tempdir().expect("home");
        let project = home.path().join("project");
        fs::create_dir_all(&project).expect("project dir");

        let mut enrichment = Enrichment::new();
        enrichment.insert("workflowId".to_string(), "wf-explicit".to_string());
        let spawn = SystemTime::now();

        let written = write_pending_stamp(WriteOptions {
            harness: PendingStampHarness::Codex,
            ledger_home: Some(home.path().to_path_buf()),
            cwd: project.to_string_lossy().into_owned(),
            enrichment: enrichment.clone(),
            session_dir_hint: None,
            spawn_start_ts: Some(spawn),
            spawner_pid: Some(42),
        })
        .expect("write pending stamp");

        assert!(written.file.starts_with(home.path().join("pending-stamps")));

        let mut ledger = Ledger::open(
            &home.path().join("burn.sqlite"),
            &home.path().join("content.sqlite"),
        )
        .expect("open ledger");
        let candidate = PendingStampSessionCandidate {
            harness: PendingStampHarness::Codex,
            session_id: "sess-explicit".to_string(),
            session_path: None,
            session_mtime_ms: Some(system_time_ms(spawn)),
            cwd: Some(project.to_string_lossy().into_owned()),
        };

        let resolved =
            resolve_pending_stamps_for_session_in(&mut ledger, &candidate, Some(home.path()))
                .expect("resolve pending stamp");

        assert_eq!(resolved.applied, 1);
        assert_eq!(resolved.enrichment, enrichment);
        assert!(!written.file.exists());
    }

    /// A candidate with `session_mtime_ms = i64::MAX` (corrupt / hostile FS
    /// metadata) must NOT be rejected by the mtime-causality guard, and when
    /// there is no cwd the fallback must return `true` (mtime is not "before"
    /// spawn). Before the `saturating_add` fix, `i64::MAX + 1` wraps negative
    /// in release builds (or panics in debug), inverting the comparison.
    #[test]
    fn pending_stamp_matches_saturates_i64_max_mtime() {
        // A real spawn timestamp (well in the past relative to i64::MAX).
        let spawn_ts = "2024-01-01T00:00:00.000Z";
        let record = PendingStamp {
            v: 1,
            harness: PendingStampHarness::Claude,
            spawner_pid: 1,
            spawn_start_ts: spawn_ts.to_string(),
            cwd: "/some/project".to_string(),
            enrichment: Enrichment::new(),
            session_dir_hint: None,
        };
        let candidate = PendingStampSessionCandidate {
            harness: PendingStampHarness::Claude,
            session_id: "sess-max".to_string(),
            session_path: None,
            session_mtime_ms: Some(i64::MAX),
            cwd: None, // no cwd → fallback path (line-365 guard)
        };
        // With saturating_add: i64::MAX.saturating_add(1) == i64::MAX >= spawn_ms → true
        // Without fix (wraps): i64::MAX + 1 == i64::MIN < spawn_ms → false
        assert!(
            pending_stamp_matches(&record, &candidate),
            "i64::MAX mtime must not be rejected by mtime-causality guard"
        );
    }
}
