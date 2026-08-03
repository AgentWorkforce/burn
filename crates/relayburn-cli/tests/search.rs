use assert_cmd::Command;
use predicates::prelude::*;
use relayburn_sdk::{
    ContentKind, ContentRecord, ContentRole, Ledger, LedgerOpenOptions, SourceKind,
};

fn burn() -> Command {
    Command::cargo_bin("burn").expect("burn binary")
}

fn content(session: &str, message: &str, text: &str) -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.into(),
        message_id: message.into(),
        ts: "2026-08-03T00:00:00Z".into(),
        role: ContentRole::Assistant,
        kind: ContentKind::Text,
        text: Some(text.into()),
        tool_use: None,
        tool_result: None,
    }
}

fn seeded_home(records: &[ContentRecord]) -> tempfile::TempDir {
    let home = tempfile::TempDir::new().expect("fixture ledger home");
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(home.path())).expect("open ledger");
    handle
        .raw_mut()
        .append_content(records)
        .expect("seed content");
    drop(handle);
    home
}

#[test]
fn search_human_finds_seeded_content_with_context_and_snippet() {
    let home = seeded_home(&[content(
        "ses_search_alpha",
        "msg_1",
        "a haystack containing burnsearchneedle",
    )]);

    burn()
        .args([
            "--no-color",
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "burnsearchneedle",
            "--snippet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ses_search_alpha"))
        .stdout(predicate::str::contains("msg_1"))
        .stdout(predicate::str::contains("burnsearchneedle"))
        .stdout(predicate::str::contains("<b>").not());
}

#[test]
fn search_no_hits_is_a_successful_clear_result() {
    let home = seeded_home(&[content("ses_search_alpha", "msg_1", "haystack")]);

    burn()
        .args([
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "absenttoken",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no content matches \"absenttoken\"",
        ));
}

#[test]
fn search_json_has_stable_camel_case_sdk_shape() {
    let home = seeded_home(&[content(
        "ses_search_alpha",
        "msg_1",
        "burnsearchneedle appears here",
    )]);
    let output = burn()
        .args([
            "--json",
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "burnsearchneedle",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("search JSON is valid");
    assert_eq!(value["query"], "burnsearchneedle");
    assert_eq!(value["limit"], 25);
    assert_eq!(value["truncated"], false);
    assert!(value["session"].is_null());
    let hits = value["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["sessionId"], "ses_search_alpha");
    assert_eq!(hits[0]["messageId"], "msg_1");
    assert_eq!(hits[0]["source"], "claude-code");
    assert!(hits[0]["rank"].is_number());
    assert!(hits[0]["snippet"]
        .as_str()
        .expect("snippet string")
        .contains("<b>"));
    assert!(hits[0].get("session_id").is_none());
}

#[test]
fn search_limit_caps_hits() {
    let home = seeded_home(&[
        content("ses_search_alpha", "msg_1", "burnsearchneedle one"),
        content("ses_search_beta", "msg_2", "burnsearchneedle two"),
        content("ses_search_gamma", "msg_3", "burnsearchneedle three"),
    ]);
    let output = burn()
        .args([
            "--json",
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "burnsearchneedle",
            "--limit",
            "2",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["hits"].as_array().unwrap().len(), 2);
    assert_eq!(value["limit"], 2);
    assert_eq!(value["truncated"], true);
}

#[test]
fn search_session_filter_scopes_hits() {
    let home = seeded_home(&[
        content("ses_search_alpha", "msg_1", "burnsearchneedle one"),
        content("ses_search_beta", "msg_2", "burnsearchneedle two"),
    ]);
    let output = burn()
        .args([
            "--json",
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "burnsearchneedle",
            "--session",
            "ses_search_beta",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = value["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["sessionId"], "ses_search_beta");
    assert_eq!(value["session"], "ses_search_beta");
}

#[test]
fn search_rejects_zero_limit_and_invalid_fts_without_panicking() {
    let home = seeded_home(&[]);
    burn()
        .args([
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "needle",
            "--limit",
            "0",
        ])
        .assert()
        .failure();
    burn()
        .args([
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "\"unbalanced",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("content search failed"));

    burn()
        .args([
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "needle",
            "--session",
            "../not-a-session",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid session id"));
}

#[test]
fn search_reports_an_unavailable_content_store_without_panicking() {
    let home = tempfile::TempDir::new().expect("fixture ledger home");
    std::fs::write(home.path().join("content.sqlite"), b"not a sqlite database")
        .expect("write corrupt content store fixture");

    burn()
        .args([
            "--ledger-path",
            home.path().to_str().unwrap(),
            "search",
            "needle",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "unable to open ledger/content store",
        ));
}
