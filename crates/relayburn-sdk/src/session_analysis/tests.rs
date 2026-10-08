//! `analyze_session` end to end over fixture sessions staged into temp
//! directories; nothing reads the real harness homes.

use std::path::{Path, PathBuf};

use super::*;
use crate::WasteSeverity;

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(path)
}

fn by_path(harness: Harness, path: PathBuf) -> SessionAnalysis {
    analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
        harness,
        path,
    }))
    .expect("analyze fixture")
}

/// A provider home holding `fixture` as a Claude project transcript.
fn claude_home(fixture_name: &str) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join(".claude/projects/-tmp-project");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join(fixture_name.rsplit('/').next().unwrap());
    std::fs::copy(fixture(fixture_name), &transcript).unwrap();
    (home, transcript)
}

fn finding<'a>(analysis: &'a SessionAnalysis, code: &str) -> &'a Finding {
    analysis
        .findings
        .iter()
        .find(|f| f.code == code)
        .unwrap_or_else(|| panic!("no {code} finding in {:#?}", analysis.findings))
}

#[test]
fn claude_transcript_by_path_explains_its_retry_loop() {
    let path = fixture("claude/retry-loop.jsonl");
    let analysis = by_path(Harness::ClaudeCode, path.clone());

    assert_eq!(analysis.schema, SESSION_ANALYSIS_SCHEMA);
    assert_eq!(analysis.session.session_id, "retry-session");
    assert_eq!(
        analysis.session.transcript_path.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
    assert_eq!(analysis.session.turn_count, 4);
    let metrics = analysis.metrics.data().expect("metrics");
    assert_eq!(metrics.usage.total_tokens, 790);
    assert!(metrics.cost_usd_micros.is_some());

    let retry = finding(&analysis, "retry-loop");
    assert_eq!(
        retry.evidence.turn_ids,
        ["msg_retry_1", "msg_retry_2", "msg_retry_3", "msg_retry_4"]
    );
    assert_eq!(retry.evidence.tools, ["Bash"]);
    assert_eq!(retry.evidence.targets, ["npm run build"]);
    assert!(retry.explanation.contains("resends the whole conversation"));
    assert!(!retry.suggestion.is_empty());
    assert_eq!(retry.impact.tokens, Some(790));
    assert!(retry.impact.cost_usd.is_some());

    let tools = &analysis.activity.data().expect("activity").tools;
    assert_eq!(
        (tools[0].tool.as_str(), tools[0].calls, tools[0].errors),
        ("Bash", 4, 4)
    );
    assert!(analysis.hotspots.data().is_some());
    assert_eq!(analysis.flow.data().expect("flow").inferences, 4);
    assert!(analysis.subagents.data().is_some());
}

#[test]
fn staged_transcripts_skip_installed_surface_checks() {
    let analysis = by_path(Harness::ClaudeCode, fixture("claude/simple-turn.jsonl"));
    assert_eq!(analysis.skipped_checks.len(), 1);
    assert_eq!(analysis.skipped_checks[0].check, "ghost-surface");
}

#[test]
fn transcript_inside_a_claude_install_is_read_in_place() {
    let (_home, transcript) = claude_home("claude/simple-turn.jsonl");
    let analysis = by_path(Harness::ClaudeCode, transcript);
    assert!(analysis.skipped_checks.is_empty());
    assert_eq!(analysis.session.turn_count, 1);
}

#[test]
fn session_id_resolves_through_a_relayhistory_store() {
    let (home, transcript) = claude_home("claude/retry-loop.jsonl");
    let mut options = AnalyzeSessionOptions::new(SessionLocator::Id {
        harness: Harness::ClaudeCode,
        session_id: "retry-session".into(),
    });
    options.store = HistoryStoreOptions {
        db_path: Some(home.path().join("ai-history.db")),
        home: Some(home.path().to_path_buf()),
    };
    let analysis = analyze_session(options.clone()).expect("analyze by id");
    assert_eq!(analysis.session.session_id, "retry-session");
    assert_eq!(
        analysis.session.transcript_path.as_deref(),
        Some(transcript.to_string_lossy().as_ref())
    );
    assert!(analysis.skipped_checks.is_empty());
    finding(&analysis, "retry-loop");

    // A second analysis reuses the store's catalog.
    assert_eq!(analyze_session(options).unwrap().session.turn_count, 4);
}

#[test]
fn unknown_session_id_names_the_store_it_searched() {
    let home = tempfile::tempdir().unwrap();
    let mut options = AnalyzeSessionOptions::new(SessionLocator::Id {
        harness: Harness::Codex,
        session_id: "missing".into(),
    });
    options.store = HistoryStoreOptions {
        db_path: Some(home.path().join("ai-history.db")),
        home: Some(home.path().to_path_buf()),
    };
    let error = format!("{:#}", analyze_session(options).unwrap_err());
    assert!(
        error.contains("codex session missing is not in the relayhistory store"),
        "{error}"
    );
}

#[test]
fn overhead_prices_the_project_instruction_files() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("CLAUDE.md"),
        format!(
            "# Build\n\n{}\n\n# Style\n\n{}\n",
            "Run the full build before committing. ".repeat(6),
            "Prefer small functions. ".repeat(4)
        ),
    )
    .unwrap();
    let mut options = AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::ClaudeCode,
        path: fixture("claude/retry-loop.jsonl"),
    });
    options.project_dir = Some(project.path().to_path_buf());
    let analysis = analyze_session(options).unwrap();

    let overhead = analysis.overhead.data().expect("overhead section");
    assert_eq!(overhead.attribution.files.len(), 1);
    assert!(overhead.attribution.grand_total > 0.0);
    assert!(!overhead.trim.recommendations.is_empty());
    let trim = finding(&analysis, "instruction-overhead");
    assert_eq!(trim.evidence.files, ["CLAUDE.md"]);
    assert!(trim
        .explanation
        .contains("re-read from cache on every turn"));
}

#[test]
fn unavailable_sections_say_why() {
    let analysis = by_path(Harness::Codex, fixture("codex/with-tool-call.jsonl"));
    assert_eq!(analysis.session.session_id, "sess_tools_1");
    assert_eq!(
        analysis.overhead.reason(),
        Some("project directory /tmp/project does not exist on this machine")
    );
    assert_eq!(
        analysis.stop_reasons.reason(),
        Some("codex transcripts record no stop reason for these turns")
    );

    let project = tempfile::tempdir().unwrap();
    let mut options = AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::ClaudeCode,
        path: fixture("claude/simple-turn.jsonl"),
    });
    options.project_dir = Some(project.path().to_path_buf());
    let analysis = analyze_session(options).unwrap();
    let reason = analysis
        .overhead
        .reason()
        .expect("no instruction files to price");
    assert!(
        reason.starts_with("no project instruction files"),
        "{reason}"
    );
}

#[test]
fn unpriced_models_never_report_zero_cost() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("unpriced.jsonl");
    let text = std::fs::read_to_string(fixture("claude/retry-loop.jsonl"))
        .unwrap()
        .replace("claude-sonnet-4-6", "unpriced-house-model");
    std::fs::write(&input, text).unwrap();
    let analysis = by_path(Harness::ClaudeCode, input);

    assert_eq!(analysis.metrics.data().unwrap().cost_usd_micros, None);
    assert!(analysis
        .activity
        .data()
        .unwrap()
        .categories
        .iter()
        .all(|row| row.cost_usd.is_none()));
    let unpriced = finding(&analysis, "unpriced-usage");
    assert_eq!(unpriced.impact.cost_usd, None);
    assert_eq!(unpriced.evidence.models, ["unpriced-house-model"]);
    assert_eq!(finding(&analysis, "retry-loop").impact.cost_usd, None);
}

#[test]
fn opencode_session_metadata_names_its_storage_tree() {
    let analysis = by_path(
        Harness::Opencode,
        fixture("opencode/multi-turn/storage/session/global/ses_multi.json"),
    );
    assert_eq!(analysis.session.session_id, "ses_multi");

    let dir = tempfile::tempdir().unwrap();
    let loose = dir.path().join("ses_multi.json");
    std::fs::write(&loose, r#"{"id":"ses_multi"}"#).unwrap();
    let error = analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::Opencode,
        path: loose,
    }))
    .unwrap_err();
    assert!(format!("{error:#}").contains("inside its storage tree"));
}

#[test]
fn findings_rank_most_severe_first_and_round_trip() {
    let analysis = by_path(
        Harness::ClaudeCode,
        fixture("claude/oversized-bash-output.jsonl"),
    );
    let bloat = finding(&analysis, "tool-output-bloat");
    assert_eq!(bloat.severity, WasteSeverity::Warn);
    assert_eq!(analysis.findings[0].code, "tool-output-bloat");

    let json = serde_json::to_value(&analysis).unwrap();
    assert_eq!(json["metrics"]["status"], "available");
    assert_eq!(json["overhead"]["status"], "unavailable");
    let back: SessionAnalysis = serde_json::from_value(json).unwrap();
    assert_eq!(back.findings, analysis.findings);
}

#[test]
fn locators_deserialize_from_camel_case() {
    let options: AnalyzeSessionOptions = serde_json::from_value(serde_json::json!({
        "session": { "by": "id", "harness": "codex", "sessionId": "s" },
        "store": { "dbPath": "/tmp/db" },
    }))
    .unwrap();
    assert_eq!(
        options.session,
        SessionLocator::Id {
            harness: Harness::Codex,
            session_id: "s".into()
        }
    );
    assert_eq!(options.store.db_path, Some(PathBuf::from("/tmp/db")));
}
