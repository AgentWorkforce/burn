use super::*;
use crate::reader::{ContentKind, ContentRole, SourceKind};

fn record() -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: "s1".to_string(),
        message_id: "m1".to_string(),
        ts: "2026-01-02T00:00:00Z".to_string(),
        role: ContentRole::Assistant,
        kind: ContentKind::Text,
        text: Some("hi".to_string()),
        tool_use: None,
        tool_result: None,
    }
}

fn since(ts: &str) -> Query {
    Query {
        since: Some(ts.to_string()),
        ..Default::default()
    }
}

fn until(ts: &str) -> Query {
    Query {
        until: Some(ts.to_string()),
        ..Default::default()
    }
}

#[test]
fn content_passes_with_empty_query() {
    assert!(content_passes(&record(), &Query::default()));
}

#[test]
fn content_passes_since_is_inclusive_lower_bound() {
    let r = record();
    assert!(content_passes(&r, &since("2026-01-01T00:00:00Z")));
    assert!(content_passes(&r, &since("2026-01-02T00:00:00Z")));
    assert!(!content_passes(&r, &since("2026-01-03T00:00:00Z")));
}

#[test]
fn content_passes_until_is_inclusive_upper_bound() {
    let r = record();
    assert!(content_passes(&r, &until("2026-01-03T00:00:00Z")));
    assert!(content_passes(&r, &until("2026-01-02T00:00:00Z")));
    assert!(!content_passes(&r, &until("2026-01-01T00:00:00Z")));
}

#[test]
fn content_passes_matches_session_and_source() {
    let r = record();
    assert!(content_passes(&r, &Query::for_session("s1")));
    assert!(!content_passes(&r, &Query::for_session("s2")));
    let source = |s| Query {
        source: Some(s),
        ..Default::default()
    };
    assert!(content_passes(&r, &source(SourceKind::Codex)));
    assert!(!content_passes(&r, &source(SourceKind::ClaudeCode)));
}

#[test]
fn content_passes_requires_every_filter() {
    let r = record();
    let all = Query {
        since: Some("2026-01-01T00:00:00Z".to_string()),
        until: Some("2026-01-03T00:00:00Z".to_string()),
        session_id: Some("s1".to_string()),
        source: Some(SourceKind::Codex),
        ..Default::default()
    };
    assert!(content_passes(&r, &all));
    let wrong_source = Query {
        source: Some(SourceKind::Opencode),
        ..all.clone()
    };
    assert!(!content_passes(&r, &wrong_source));
}
