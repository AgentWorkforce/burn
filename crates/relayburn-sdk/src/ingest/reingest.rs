//! `reingest_missing_content` — Rust port of TS
//! `reingestMissingContent` from `packages/ingest/src/ingest.ts`.
//!
//! Re-parses upstream session source files to populate missing
//! `content` rows and `user_turn` rows. Used by `burn state rebuild
//! content` to fix up historical sessions ingested before those derived
//! records were written (or where the sidecar was pruned). Does NOT
//! touch ingest cursors, ledger turns, or compaction events.
//!
//! ## Skip filter
//!
//! A session is considered "fully covered" iff its session id is present
//! in BOTH `content.sqlite` (via [`Ledger::list_content_session_ids`])
//! and the `user_turns` table (via
//! [`Ledger::list_user_turn_session_ids`]). The TS surface's skip filter
//! has the same shape: a session is only skipped when both sides have
//! something for it. Anything missing on either side triggers a
//! re-parse pass.
//!
//! ## Rust port deviations from TS
//!
//! * The TS path lives in one big function in `ingest.ts`; we lift it to
//!   its own module so the per-harness orchestration helpers (#277) and
//!   the gap state machine (#278) stay self-contained.
//! * The TS path always parses with `contentMode: 'full'`; we follow
//!   suit, regardless of `BurnConfig`'s configured mode. This is the
//!   right behaviour because the verb's whole purpose is to backfill
//!   content even when the running ingest is in a non-Full mode.

use std::path::{Path, PathBuf};

use crate::ledger::Ledger;
use crate::reader::{
    parse_claude_session_incremental, parse_codex_session_incremental,
    parse_opencode_session_incremental, read_codex_session_id_hint, ClaudeParseIncrementalOptions,
    ContentRecord, ContentStoreMode, ParseCodexIncrementalOptions, ParseOpencodeIncrementalOptions,
    UserTurnRecord,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

use crate::ingest::ingest::{
    claude_projects_dir, codex_sessions_dir, opencode_message_root, opencode_session_root,
    IngestOptions, IngestRoots,
};
use crate::ingest::walk::{list_dirs, list_jsonl_files, walk_jsonl, walk_opencode_sessions};

/// Outcome of [`reingest_missing_content`]. Mirrors the TS
/// `ReingestContentReport` shape one-to-one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReingestContentReport {
    pub scanned_files: usize,
    pub skipped_existing: usize,
    pub reingested_sessions: usize,
    pub appended_content: usize,
    pub appended_user_turns: usize,
    pub failed: usize,
}

/// Re-parse source session files to populate missing content sidecars
/// and user-turn rows. Used by `burn state rebuild content` to fix up
/// historical sessions ingested before those derived records were
/// written (or where the sidecar was pruned). Does NOT touch ingest
/// cursors, ledger turns, or compaction events.
///
/// Mirrors TS `reingestMissingContent`.
pub fn reingest_missing_content(
    ledger: &mut Ledger,
    opts: &IngestOptions,
) -> anyhow::Result<ReingestContentReport> {
    progress(opts, "loading existing content records");
    let mut existing_content = ledger.list_content_session_ids()?;
    progress(opts, "loading existing user-turn records");
    let mut existing_user_turns = ledger.list_user_turn_session_ids()?;
    let mut report = ReingestContentReport::default();
    progress(opts, "re-parsing Claude Code sessions for content");
    reingest_claude_content(
        ledger,
        &opts.roots,
        &mut existing_content,
        &mut existing_user_turns,
        &mut report,
    )?;
    progress(opts, "re-parsing Codex sessions for content");
    reingest_codex_content(
        ledger,
        &opts.roots,
        &mut existing_content,
        &mut existing_user_turns,
        &mut report,
    )?;
    progress(opts, "re-parsing OpenCode sessions for content");
    reingest_opencode_content(
        ledger,
        &opts.roots,
        &mut existing_content,
        &mut existing_user_turns,
        &mut report,
    )?;
    Ok(report)
}

fn progress(opts: &IngestOptions, msg: &str) {
    if let Some(cb) = &opts.on_progress {
        cb(msg);
    }
}

fn reingest_claude_content(
    ledger: &mut Ledger,
    roots: &IngestRoots,
    existing_content: &mut HashSet<String>,
    existing_user_turns: &mut HashSet<String>,
    report: &mut ReingestContentReport,
) -> anyhow::Result<()> {
    let projects = list_dirs(&claude_projects_dir(roots));
    for project_dir in projects {
        let files = list_jsonl_files(&project_dir);
        for file in files {
            report.scanned_files += 1;
            let session_id = derive_claude_session_id(&file);
            if let Some(sid) = session_id.as_deref() {
                if existing_content.contains(sid) && existing_user_turns.contains(sid) {
                    report.skipped_existing += 1;
                    continue;
                }
            }
            let opts = ClaudeParseIncrementalOptions {
                start_offset: Some(0),
                session_path: Some(file.to_string_lossy().into_owned()),
                content_mode: Some(ContentStoreMode::Full),
                ..Default::default()
            };
            match parse_claude_session_incremental(&file, &opts) {
                Ok(parsed) => {
                    append_reingested_derived_records(
                        ledger,
                        &parsed.content,
                        &parsed.user_turns,
                        existing_content,
                        existing_user_turns,
                        report,
                    )?;
                }
                Err(err) => {
                    report.failed += 1;
                    eprintln!("[burn] reingest skipped {}: {err}", file.display());
                }
            }
        }
    }
    Ok(())
}

fn reingest_codex_content(
    ledger: &mut Ledger,
    roots: &IngestRoots,
    existing_content: &mut HashSet<String>,
    existing_user_turns: &mut HashSet<String>,
    report: &mut ReingestContentReport,
) -> anyhow::Result<()> {
    for file in walk_jsonl(codex_sessions_dir(roots)) {
        report.scanned_files += 1;
        let derived = derive_codex_session_id(&file);
        if let Some(sid) = derived.as_deref() {
            if existing_content.contains(sid) && existing_user_turns.contains(sid) {
                report.skipped_existing += 1;
                continue;
            }
        }
        let opts = ParseCodexIncrementalOptions {
            start_offset: Some(0),
            session_path: Some(file.to_string_lossy().into_owned()),
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        };
        match parse_codex_session_incremental(&file, &opts) {
            Ok(parsed) => {
                append_reingested_derived_records(
                    ledger,
                    &parsed.content,
                    &parsed.user_turns,
                    existing_content,
                    existing_user_turns,
                    report,
                )?;
            }
            Err(err) => {
                report.failed += 1;
                eprintln!("[burn] reingest skipped {}: {err}", file.display());
            }
        }
    }
    Ok(())
}

fn reingest_opencode_content(
    ledger: &mut Ledger,
    roots: &IngestRoots,
    existing_content: &mut HashSet<String>,
    existing_user_turns: &mut HashSet<String>,
    report: &mut ReingestContentReport,
) -> anyhow::Result<()> {
    let session_root = opencode_session_root(roots);
    let _message_root = opencode_message_root(roots);
    for file in walk_opencode_sessions(&session_root) {
        report.scanned_files += 1;
        let session_id = file
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        if let Some(sid) = session_id.as_deref() {
            if existing_content.contains(sid) && existing_user_turns.contains(sid) {
                report.skipped_existing += 1;
                continue;
            }
        }
        let opts = ParseOpencodeIncrementalOptions {
            session_path: Some(file.to_string_lossy().into_owned()),
            content_mode: Some(ContentStoreMode::Full),
            seen_message_ids: Some(BTreeSet::new()),
            ..Default::default()
        };
        match parse_opencode_session_incremental(&file, &opts) {
            Ok(parsed) => {
                append_reingested_derived_records(
                    ledger,
                    &parsed.content,
                    &parsed.user_turns,
                    existing_content,
                    existing_user_turns,
                    report,
                )?;
            }
            Err(err) => {
                report.failed += 1;
                eprintln!("[burn] reingest skipped {}: {err}", file.display());
            }
        }
    }
    Ok(())
}

fn append_reingested_derived_records(
    ledger: &mut Ledger,
    content: &[ContentRecord],
    user_turns: &[UserTurnRecord],
    existing_content: &mut HashSet<String>,
    existing_user_turns: &mut HashSet<String>,
    report: &mut ReingestContentReport,
) -> anyhow::Result<()> {
    let filtered_content: Vec<ContentRecord> = content
        .iter()
        .filter(|c| !existing_content.contains(&c.session_id))
        .cloned()
        .collect();
    let filtered_user_turns: Vec<UserTurnRecord> = user_turns
        .iter()
        .filter(|u| !existing_user_turns.contains(&u.session_id))
        .cloned()
        .collect();
    if filtered_content.is_empty() && filtered_user_turns.is_empty() {
        return Ok(());
    }

    if !filtered_content.is_empty() {
        ledger.append_content(&filtered_content)?;
        report.appended_content += filtered_content.len();
        for c in &filtered_content {
            existing_content.insert(c.session_id.clone());
        }
    }
    if !filtered_user_turns.is_empty() {
        ledger.append_user_turns(&filtered_user_turns)?;
        report.appended_user_turns += filtered_user_turns.len();
        for u in &filtered_user_turns {
            existing_user_turns.insert(u.session_id.clone());
        }
    }

    let mut sessions: HashSet<&str> = HashSet::new();
    for c in &filtered_content {
        sessions.insert(c.session_id.as_str());
    }
    for u in &filtered_user_turns {
        sessions.insert(u.session_id.as_str());
    }
    report.reingested_sessions += sessions.len();
    Ok(())
}

/// Codex filenames are `rollout-<timestamp>-<uuid>.jsonl` where the
/// UUID is the session id. Extract it for a cheap skip check before
/// parsing. If the pattern doesn't match, peek at Codex's first-line
/// `session_meta` hint before falling back to post-filtering.
///
/// Mirrors TS `deriveCodexSessionId`.
pub fn derive_codex_session_id(file: &Path) -> Option<String> {
    let stem = file.file_stem().and_then(|s| s.to_str())?;
    if let Some(uuid) = trailing_uuid(stem) {
        return Some(uuid);
    }
    read_codex_session_id_hint(file)
}

fn trailing_uuid(stem: &str) -> Option<String> {
    // Match the TS regex
    // `/([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/`.
    if stem.len() < 36 {
        return None;
    }
    let candidate = &stem[stem.len() - 36..];
    let bytes = candidate.as_bytes();
    let dash_positions = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if dash_positions.contains(&i) {
            if *b != b'-' {
                return None;
            }
        } else if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(candidate.to_string())
}

/// Claude session files are `<sessionId>.jsonl` under the encoded-cwd
/// project dir. The basename is the session id.
fn derive_claude_session_id(file: &Path) -> Option<String> {
    file.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::ledger::{Ledger, LedgerLayout};
    use crate::reader::{
        ContentKind, ContentRecord, ContentRole, SourceKind, UserTurnBlock, UserTurnBlockKind,
        UserTurnRecord,
    };

    use crate::ingest::ingest::{IngestOptions, IngestRoots};

    fn open_ledger(tmp: &TempDir) -> Ledger {
        let layout = LedgerLayout::under(tmp.path());
        Ledger::open(&layout.burn, &layout.content).unwrap()
    }

    fn opts_with_roots(roots: IngestRoots) -> IngestOptions {
        IngestOptions {
            roots,
            ..Default::default()
        }
    }

    fn fake_content(session: &str, message: &str) -> ContentRecord {
        ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.into(),
            message_id: message.into(),
            ts: "2026-04-22T00:00:00.000Z".into(),
            role: ContentRole::Assistant,
            kind: ContentKind::Text,
            text: Some("seed".into()),
            tool_use: None,
            tool_result: None,
        }
    }

    fn fake_user_turn(session: &str, user_uuid: &str) -> UserTurnRecord {
        UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.into(),
            user_uuid: user_uuid.into(),
            ts: "2026-04-22T00:00:00.000Z".into(),
            preceding_message_id: None,
            following_message_id: None,
            blocks: vec![UserTurnBlock {
                kind: UserTurnBlockKind::Text,
                tool_use_id: None,
                byte_len: 4,
                approx_tokens: 1,
                is_error: None,
            }],
        }
    }

    /// Write a minimal Claude session JSONL file with one user prompt
    /// + one assistant text reply. The parser produces a `text`
    /// ContentRecord and one UserTurnRecord; both should land via
    /// `reingest_missing_content`.
    fn write_claude_session(claude_root: &Path, session_id: &str) -> PathBuf {
        let project_dir = claude_root.join("-tmp-project");
        fs::create_dir_all(&project_dir).unwrap();
        let file = project_dir.join(format!("{session_id}.jsonl"));
        let lines: Vec<String> = vec![
            serde_json::json!({
                "type": "permission-mode",
                "permissionMode": "default",
                "sessionId": session_id,
            })
            .to_string(),
            serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "promptId": "p-1",
                "type": "user",
                "message": { "role": "user", "content": "please fix the build" },
                "uuid": "u-user-1",
                "timestamp": "2026-04-20T00:00:00.000Z",
                "cwd": "/tmp/project",
                "sessionId": session_id,
                "version": "2.1.96",
            })
            .to_string(),
            serde_json::json!({
                "parentUuid": "u-user-1",
                "isSidechain": false,
                "message": {
                    "model": "claude-sonnet-4-6",
                    "id": "msg_ut_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Hello!" }],
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 500,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 80,
                            "ephemeral_1h_input_tokens": 20,
                        },
                        "output_tokens": 5,
                        "service_tier": "standard",
                    },
                },
                "requestId": "req_1",
                "type": "assistant",
                "uuid": "u-asst-1",
                "timestamp": "2026-04-20T00:00:01.000Z",
                "cwd": "/tmp/project",
                "sessionId": session_id,
                "version": "2.1.96",
            })
            .to_string(),
        ];
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(&file, body).unwrap();
        file
    }

    #[test]
    fn skips_session_with_both_content_and_user_turn_already_present() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_ledger(&tmp);

        let home = TempDir::new().unwrap();
        let claude_root = home.path().join(".claude").join("projects");
        fs::create_dir_all(&claude_root).unwrap();
        let session_id = "44444444-4444-4444-4444-444444444444";
        write_claude_session(&claude_root, session_id);

        // Pre-seed both sides: one content row + one user-turn row for
        // this session, so the AND-skip filter should bypass the parse.
        ledger
            .append_content(&[fake_content(session_id, "seed-content")])
            .unwrap();
        ledger
            .append_user_turns(&[fake_user_turn(session_id, "seed-user")])
            .unwrap();

        let opts = opts_with_roots(IngestRoots {
            claude_projects_dir: Some(claude_root.clone()),
            codex_sessions_dir: Some(home.path().join(".codex").join("sessions")),
            opencode_storage_dir: Some(home.path().join(".local/share/opencode/storage")),
        });

        let report = reingest_missing_content(&mut ledger, &opts).unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.skipped_existing, 1);
        assert_eq!(report.reingested_sessions, 0);
        assert_eq!(report.appended_content, 0);
        assert_eq!(report.appended_user_turns, 0);
    }

    #[test]
    fn reparses_session_with_no_existing_records() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_ledger(&tmp);

        let home = TempDir::new().unwrap();
        let claude_root = home.path().join(".claude").join("projects");
        fs::create_dir_all(&claude_root).unwrap();
        let session_id = "55555555-5555-5555-5555-555555555555";
        write_claude_session(&claude_root, session_id);

        let opts = opts_with_roots(IngestRoots {
            claude_projects_dir: Some(claude_root.clone()),
            codex_sessions_dir: Some(home.path().join(".codex").join("sessions")),
            opencode_storage_dir: Some(home.path().join(".local/share/opencode/storage")),
        });

        let report = reingest_missing_content(&mut ledger, &opts).unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.skipped_existing, 0);
        assert!(report.appended_user_turns >= 1);
        assert!(report.reingested_sessions >= 1);

        // After the run, the AND-skip filter sees both sides covered, so
        // a re-run skips the same file.
        let report2 = reingest_missing_content(&mut ledger, &opts).unwrap();
        assert_eq!(report2.scanned_files, 1);
        assert_eq!(report2.skipped_existing, 1);
        assert_eq!(report2.appended_content, 0);
        assert_eq!(report2.appended_user_turns, 0);
    }

    #[test]
    fn backfills_user_turn_when_only_content_exists() {
        // Mirrors the TS "rebuild content backfills user-turn rows even
        // when content already exists" case: pre-seed the content side
        // only and verify the verb appends a user-turn row even though
        // it skips the content append (filtered out).
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_ledger(&tmp);

        let home = TempDir::new().unwrap();
        let claude_root = home.path().join(".claude").join("projects");
        fs::create_dir_all(&claude_root).unwrap();
        let session_id = "66666666-6666-6666-6666-666666666666";
        write_claude_session(&claude_root, session_id);

        // Pre-seed content only — user_turns side stays empty.
        ledger
            .append_content(&[fake_content(session_id, "seed-content")])
            .unwrap();

        let opts = opts_with_roots(IngestRoots {
            claude_projects_dir: Some(claude_root.clone()),
            codex_sessions_dir: Some(home.path().join(".codex").join("sessions")),
            opencode_storage_dir: Some(home.path().join(".local/share/opencode/storage")),
        });

        let report = reingest_missing_content(&mut ledger, &opts).unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.skipped_existing, 0);
        // Content appended should be 0 (existing_content has the
        // session id, so re-parsed content is filtered out). User-turn
        // appended should be >= 1.
        assert_eq!(report.appended_content, 0);
        assert!(report.appended_user_turns >= 1);
    }

    #[test]
    fn missing_session_dirs_are_silently_skipped() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_ledger(&tmp);

        let home = TempDir::new().unwrap();
        let opts = opts_with_roots(IngestRoots {
            claude_projects_dir: Some(home.path().join(".claude").join("projects")),
            codex_sessions_dir: Some(home.path().join(".codex").join("sessions")),
            opencode_storage_dir: Some(home.path().join(".local/share/opencode/storage")),
        });

        let report = reingest_missing_content(&mut ledger, &opts).unwrap();
        assert_eq!(report, ReingestContentReport::default());
    }

    #[test]
    fn derive_codex_session_id_recognizes_canonical_filenames() {
        let p = PathBuf::from(
            "/x/rollout-2026-04-24T00-00-00-000Z-11111111-2222-3333-4444-555555555555.jsonl",
        );
        assert_eq!(
            derive_codex_session_id(&p),
            Some("11111111-2222-3333-4444-555555555555".to_string())
        );
    }

    #[test]
    fn derive_codex_session_id_returns_none_for_opaque_filename_when_file_missing() {
        let p = PathBuf::from("/tmp/this/path/should/not/exist-opaque.jsonl");
        assert_eq!(derive_codex_session_id(&p), None);
    }
}
