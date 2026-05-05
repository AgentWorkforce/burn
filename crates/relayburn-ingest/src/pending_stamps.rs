//! Pending-stamp coordination — Rust port of `packages/ingest/src/pending-stamps.ts`.
//!
//! Wrapper harnesses (`burn run codex`, `burn run opencode`) that spawn a
//! child process before the session id is known drop a JSON manifest into
//! `$RELAYBURN_HOME/pending-stamps/`. After the child exits, the next ingest
//! pass tries to match each manifest against a freshly-discovered session and
//! folds the manifest's enrichment into the ledger via `Ledger::append_stamp`.
//!
//! ## Wire-format compatibility
//!
//! The on-disk JSON shape is binary-compatible with the TS adapter so a
//! Rust-resident watch loop and a TS-resident `burn run` wrapper can coexist
//! during the migration. Specifically:
//!
//! * Object keys are emitted in insertion order: `v`, `harness`, `spawnerPid`,
//!   `spawnStartTs`, `cwd`, `enrichment`, then optional `sessionDirHint`.
//! * The file is `JSON.stringify(record, null, 2) + '\n'` — pretty-printed
//!   with two-space indent and a trailing newline, matching `node:fs`.
//! * The filename pattern is `<harness>-<spawnerPid>-<spawnStartMs>-<uuid>.json`
//!   in `$RELAYBURN_HOME/pending-stamps/`. In-flight writes go through a
//!   `.tmp-<pid>-<uuid>` sibling and are atomically renamed.
//! * Claimed manifests are renamed to `<file>.claimed-<pid>-<uuid>` before the
//!   ledger append, then unlinked on success or restored on failure.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relayburn_ledger::{ledger_home, Enrichment, Ledger, Stamp, StampSelector};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
    Opencode,
}

impl PendingStampHarness {
    fn as_str(self) -> &'static str {
        match self {
            PendingStampHarness::Codex => "codex",
            PendingStampHarness::Opencode => "opencode",
        }
    }
}

/// Parsed manifest. Fields are ordered to match the TS object-literal
/// insertion order so re-serialisation is byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStamp {
    pub v: u8,
    pub harness: PendingStampHarness,
    pub spawner_pid: u32,
    pub spawn_start_ts: String,
    pub cwd: String,
    pub enrichment: Enrichment,
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

/// Write a manifest for a freshly-spawned harness child. Cleanup runs first
/// so old stamps don't leak into the matcher.
pub fn write_pending_stamp(opts: WriteOptions) -> std::io::Result<PendingStampWriteResult> {
    let spawn_start = opts.spawn_start_ts.unwrap_or_else(SystemTime::now);
    cleanup_stale_pending_stamps_at(spawn_start, PENDING_STAMP_TTL_MS)?;

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

    let dir = pending_stamps_dir();
    fs::create_dir_all(&dir)?;
    let spawn_ms = system_time_ms(spawn_start);
    let uuid = uuid_v4();
    let base = format!(
        "{}-{}-{}-{}",
        stamp.harness.as_str(),
        stamp.spawner_pid,
        spawn_ms,
        uuid
    );
    let final_path = dir.join(format!("{base}.json"));
    let tmp_path = dir.join(format!("{base}.tmp-{}-{}", std::process::id(), uuid_v4()));

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

pub fn cleanup_stale_pending_stamps_at(
    now: SystemTime,
    ttl_ms: u64,
) -> std::io::Result<PendingStampCleanupResult> {
    let now_ms = system_time_ms(now);
    let files = list_pending_stamp_files(false)?;
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
pub fn resolve_pending_stamps_for_session(
    ledger: &mut Ledger,
    candidate: &PendingStampSessionCandidate,
) -> std::io::Result<PendingStampResolveResult> {
    if candidate.session_id.is_empty() {
        return Ok(PendingStampResolveResult::default());
    }

    cleanup_stale_pending_stamps()?;
    let files = list_pending_stamp_files(true)?;
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
        if mtime + MTIME_SLOP_MS < spawn_ms {
            return false;
        }
    }
    if let Some(cwd) = &candidate.cwd {
        return canonicalize_lossy(Path::new(cwd)) == record.cwd;
    }
    // Fallback when reader cannot recover session cwd: rely on mtime causality.
    candidate
        .session_mtime_ms
        .map(|m| m + MTIME_SLOP_MS >= spawn_ms)
        .unwrap_or(false)
}

fn claim_pending_stamp(file: &Path) -> std::io::Result<Option<PathBuf>> {
    let claimed = with_suffix(
        file,
        &format!(".claimed-{}-{}", std::process::id(), uuid_v4()),
    );
    match fs::rename(file, &claimed) {
        Ok(()) => Ok(Some(claimed)),
        Err(_) => Ok(None),
    }
}

fn list_pending_stamp_files(active_only: bool) -> std::io::Result<Vec<PathBuf>> {
    let dir = pending_stamps_dir();
    let entries = match fs::read_dir(&dir) {
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
    let value: Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;
    if obj.get("v").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let harness = match obj.get("harness").and_then(Value::as_str)? {
        "codex" => PendingStampHarness::Codex,
        "opencode" => PendingStampHarness::Opencode,
        _ => return None,
    };
    let spawner_pid = obj.get("spawnerPid").and_then(Value::as_u64)? as u32;
    let spawn_start_ts = obj.get("spawnStartTs").and_then(Value::as_str)?.to_string();
    parse_iso_ms(&spawn_start_ts)?;
    let cwd = obj.get("cwd").and_then(Value::as_str)?.to_string();
    if cwd.is_empty() {
        return None;
    }
    let enrichment_raw = obj.get("enrichment").and_then(Value::as_object)?;
    let mut enrichment = BTreeMap::new();
    for (k, v) in enrichment_raw {
        let s = v.as_str()?;
        enrichment.insert(k.clone(), s.to_string());
    }
    let session_dir_hint = match obj.get("sessionDirHint") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        _ => return None,
    };
    Some(PendingStamp {
        v: 1,
        harness,
        spawner_pid,
        spawn_start_ts,
        cwd,
        enrichment,
        session_dir_hint,
    })
}

/// Serialize a manifest in the exact wire format the TS adapter writes:
/// `JSON.stringify(record, null, 2) + '\n'`. We hand-roll the object so the
/// key order is deterministic (`v`, `harness`, `spawnerPid`, `spawnStartTs`,
/// `cwd`, `enrichment`, [`sessionDirHint`]) regardless of the runtime
/// `serde_json` map type.
pub fn serialize_stamp(stamp: &PendingStamp) -> String {
    let mut map = Map::new();
    map.insert("v".into(), Value::Number(1.into()));
    map.insert(
        "harness".into(),
        Value::String(stamp.harness.as_str().to_string()),
    );
    map.insert("spawnerPid".into(), Value::Number(stamp.spawner_pid.into()));
    map.insert(
        "spawnStartTs".into(),
        Value::String(stamp.spawn_start_ts.clone()),
    );
    map.insert("cwd".into(), Value::String(stamp.cwd.clone()));
    let mut enrichment = Map::new();
    for (k, v) in &stamp.enrichment {
        enrichment.insert(k.clone(), Value::String(v.clone()));
    }
    map.insert("enrichment".into(), Value::Object(enrichment));
    if let Some(hint) = &stamp.session_dir_hint {
        map.insert("sessionDirHint".into(), Value::String(hint.clone()));
    }
    let mut s = serde_json::to_string_pretty(&Value::Object(map)).expect("serializable");
    s.push('\n');
    s
}

fn format_iso_8601(t: SystemTime) -> String {
    // Mirror JS `toISOString()` — ms-precision UTC: `2024-05-04T12:34:56.789Z`.
    let dur = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0));
    let secs = dur.as_secs() as i64;
    let ms = dur.subsec_millis();
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, ms
    )
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    // Accept the JS `Date.parse` shapes we actually emit:
    // `YYYY-MM-DDTHH:MM:SS[.fff]Z`. Anything more exotic was never written
    // by the TS adapter so we don't need to round-trip it.
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[bytes.len() - 1] != b'Z' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    if s.as_bytes().get(4) != Some(&b'-') {
        return None;
    }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if s.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let day: u32 = s.get(8..10)?.parse().ok()?;
    if s.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    let mut ms: i64 = 0;
    if s.as_bytes().get(19) == Some(&b'.') {
        let frac_end = s.len() - 1;
        let frac = s.get(20..frac_end)?;
        if !frac.is_empty() {
            let mut padded = String::from(frac);
            while padded.len() < 3 {
                padded.push('0');
            }
            ms = padded.get(0..3)?.parse().ok()?;
        }
    } else if s.as_bytes().get(19) != Some(&b'Z') {
        return None;
    }
    let secs = unix_seconds_from_civil(year, month, day, hour, minute, second);
    Some(secs * 1000 + ms)
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

/// Tiny v4-ish UUID generator. We don't need cryptographic strength here —
/// the only goal is filename uniqueness across concurrent writers in the
/// same `pending-stamps/` directory. Pulls 16 bytes of entropy from
/// `getrandom` via /dev/urandom (or the OS equivalent on Windows).
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

#[cfg(unix)]
fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    fill_random_pid_time_fallback(buf);
}

#[cfg(not(unix))]
fn fill_random(buf: &mut [u8]) {
    fill_random_pid_time_fallback(buf);
}

fn fill_random_pid_time_fallback(buf: &mut [u8]) {
    // Last-resort: nanos+pid+counter. Good enough for filename uniqueness.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = n ^ (pid << 32) ^ (c << 17);
    for b in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

// --- Civil ↔ Unix-seconds conversions (proleptic Gregorian, no chrono dep) -

/// Days since 1970-01-01 for the start of `year-month-day`. Algorithm from
/// Howard Hinnant's "date" library.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn unix_seconds_from_civil(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> i64 {
    days_from_civil(y, m, d) * 86400 + (hh as i64) * 3600 + (mm as i64) * 60 + ss as i64
}

fn civil_from_unix_seconds(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let z = secs.div_euclid(86400) + 719468;
    let sod = secs.rem_euclid(86400);
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let hh = (sod / 3600) as u32;
    let mm = ((sod % 3600) / 60) as u32;
    let ss = (sod % 60) as u32;
    (y, m, d, hh, mm, ss)
}
