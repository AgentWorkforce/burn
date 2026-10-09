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

/// A Claude install under a temp home whose only session is `retry-loop`
/// on a model burn has no price for, plus one never-used agent.
fn unpriced_install() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join(".claude/projects/-tmp-project");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join("retry-loop.jsonl");
    let text = std::fs::read_to_string(fixture("claude/retry-loop.jsonl"))
        .unwrap()
        .replace("claude-sonnet-4-6", "unpriced-house-model");
    std::fs::write(&transcript, text).unwrap();
    let agents = home.path().join(".claude/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("forgotten-helper.md"),
        "---\nname: forgotten-helper\n---\nHelps with things nobody asks for.\n",
    )
    .unwrap();
    (home, transcript)
}

#[test]
fn unpriced_sessions_explain_costs_as_unknown_and_rank_by_tokens() {
    let (_home, transcript) = unpriced_install();
    let analysis = by_path(Harness::ClaudeCode, transcript);

    let ghost = finding(&analysis, "ghost-agent");
    assert!(!ghost.suggestion.is_empty());
    assert_eq!(ghost.impact.cost_usd, None);
    for f in &analysis.findings {
        assert!(!f.suggestion.is_empty(), "{} has no suggestion", f.code);
        assert!(
            !f.explanation.contains("$0"),
            "{} explains an unpriced cost as $0: {}",
            f.code,
            f.explanation
        );
        assert_eq!(f.impact.cost_usd, None, "{}", f.code);
    }
    let tokens: Vec<u64> = analysis
        .findings
        .iter()
        .filter(|f| f.severity == WasteSeverity::Info)
        .map(|f| f.impact.tokens.unwrap_or(0))
        .collect();
    let mut sorted = tokens.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(tokens, sorted, "info findings rank by tokens when unpriced");

    // Unpriced turns still attribute tokens to the commands that caused them.
    let hotspots = analysis.hotspots.data().expect("hotspots");
    assert!(hotspots.bash[0].initial_tokens > 0.0);
}

#[test]
fn findings_with_known_impact_rank_before_unknown_impact() {
    let impact = |tokens: Option<u64>, cost_usd: Option<f64>| FindingImpact {
        tokens,
        cost_usd,
        pricing: crate::FindingPricingStatus::Priced,
    };
    let finding = |code: &str, impact: FindingImpact| Finding {
        code: code.into(),
        severity: WasteSeverity::Info,
        title: String::new(),
        explanation: String::new(),
        evidence: FindingEvidence::default(),
        impact,
        suggestion: String::new(),
        actions: Vec::new(),
    };
    let mut findings = [
        finding("none", impact(None, None)),
        finding("zero", impact(Some(0), Some(0.0))),
        finding("tokens", impact(Some(6_000), None)),
        finding("cheap", impact(Some(10), Some(0.01))),
        finding("dear", impact(Some(1), Some(0.5))),
    ];
    findings.sort_by(findings::rank);
    let order: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
    assert_eq!(order, ["dear", "cheap", "tokens", "none", "zero"]);
}

#[test]
fn every_finding_code_carries_a_suggestion() {
    let codes = crate::query_verbs::default_hotspots_finding_kinds()
        .into_iter()
        .chain(
            [
                "ghost-agent",
                "ghost-skill",
                "ghost-command",
                "instruction-overhead",
                "context-growth",
                "max-tokens-stop",
                "refusal",
                "usage-unrecorded",
                "attribution-unavailable",
            ]
            .map(String::from),
        );
    for code in codes {
        let (_, suggestion) = explain::guidance(&code);
        assert!(!suggestion.is_empty(), "{code}");
    }
}

#[test]
fn unpriced_instruction_files_still_report_their_tokens() {
    let (_home, transcript) = unpriced_install();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("CLAUDE.md"),
        format!("# Build\n\n{}\n", "Run the full build. ".repeat(6)),
    )
    .unwrap();
    let mut options = AnalyzeSessionOptions::new(SessionLocator::Path {
        harness: Harness::ClaudeCode,
        path: transcript,
    });
    options.project_dir = Some(project.path().to_path_buf());
    let analysis = analyze_session(options).unwrap();
    let trim = finding(&analysis, "instruction-overhead");
    assert!(trim.impact.tokens.unwrap() > 0);
    assert_eq!(trim.impact.cost_usd, None);
}

/// Codex evidence of three tasks whose `turn_context` is stored only when it
/// changes: `low` for the first two, `high` for the third.
fn codex_effort_evidence() -> ai_hist::SessionEvidence {
    let marker = |line: u64, kind: &str, turn: Option<&str>, payload: serde_json::Value| {
        serde_json::json!({
            "marker_uid": format!("{line}:marker"), "ts_ms": 1_790_000_000_000u64 + line * 1000,
            "message_id": null, "parent_id": null, "turn_id": turn,
            "kind": kind, "subkind": kind, "text": null, "payload": payload
        })
    };
    let context = |line: u64, turn: &str, effort: &str| {
        marker(
            line,
            "turn_context",
            Some(turn),
            serde_json::json!({"turn_id": turn, "model": "gpt-5.4", "cwd": "/tmp/project",
                               "effort": effort, "summary": "detailed"}),
        )
    };
    let snapshot = |line: u64, input: u64, output: u64, reasoning: u64| {
        marker(
            line,
            "usage_snapshot",
            None,
            serde_json::json!({"total_token_usage": {
                "input_tokens": input, "cached_input_tokens": 0,
                "output_tokens": output, "reasoning_output_tokens": reasoning
            }}),
        )
    };
    let mut markers = Vec::new();
    for (i, turn) in ["t1", "t2", "t3"].iter().enumerate() {
        let line = 10 + 10 * i as u64;
        let n = i as u64 + 1;
        markers.push(marker(
            line,
            "task_started",
            Some(turn),
            serde_json::Value::Null,
        ));
        if i != 1 {
            markers.push(context(line + 1, turn, if i == 0 { "low" } else { "high" }));
        }
        markers.push(snapshot(line + 2, 1000 * n, 100 * n * n, 50 * n * n));
        markers.push(marker(
            line + 9,
            "task_complete",
            Some(turn),
            serde_json::Value::Null,
        ));
    }
    serde_json::from_value(serde_json::json!({
        "session": {
            "source": "codex", "session_id": "effort-session", "cwd": "/tmp/project",
            "git_branch": null, "first_activity_ms": 0, "last_activity_ms": 0,
            "first_prompt": null, "last_assistant_text": null, "models": [],
            "originator": null, "agent_version": null, "repo_url": null,
            "initial_commit": null, "workspace_roots": [], "project_key": null,
            "project_key_method": null, "raw_path": null, "source_stamp": null,
            "discovery_state": "full", "locations": ["local"]
        },
        "prompts": [], "messages": [], "tool_calls": [], "tool_results": [],
        "file_edits": [], "markers": markers, "relationships": [], "requests": [],
        "usage": null, "user_turns": [], "coverage": [], "loaded": [],
        "include_text": true, "diagnostics": []
    }))
    .unwrap()
}

#[test]
fn codex_turn_context_effort_reaches_the_reasoning_section() {
    let analysis = analyze_evidence(&codex_effort_evidence(), &AnalysisSettings::default())
        .expect("analyze evidence");
    let reasoning = analysis.reasoning.data().expect("reasoning section");
    let levels: Vec<(Option<&str>, u64, u64)> = reasoning
        .levels
        .iter()
        .map(|row| (row.effort.as_deref(), row.turns, row.reasoning_tokens))
        .collect();
    // Cumulative reasoning 50, 200, 450: t2 carries t1's `low` forward.
    assert_eq!(levels, [(Some("low"), 2, 200), (Some("high"), 1, 250)]);
    assert!(reasoning.levels.iter().all(|row| row.cost_usd.is_some()));
    let change = &reasoning.changes[0];
    assert_eq!(
        (
            change.turn_id.as_str(),
            change.from.as_str(),
            change.to.as_str()
        ),
        ("t3", "low", "high")
    );
    let finding = finding(&analysis, "reasoning-effort-change");
    assert!(finding.explanation.contains("low to high at turn 2"));
    let json = serde_json::to_value(&analysis).unwrap();
    assert_eq!(json["reasoning"]["status"], "available");
    assert_eq!(
        json["reasoning"]["data"]["levels"][0]["reasoningTokens"],
        200
    );
}

#[test]
fn sessions_without_recorded_effort_leave_the_reasoning_section_unavailable() {
    let analysis = by_path(Harness::ClaudeCode, fixture("claude/retry-loop.jsonl"));
    assert_eq!(
        analysis.reasoning.reason(),
        Some("no turn of this claude-code session records a reasoning effort")
    );
}
