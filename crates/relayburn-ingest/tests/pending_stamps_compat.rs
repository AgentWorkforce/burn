//! Cross-adapter pending-stamp wire-format compatibility — acceptance test
//! for AgentWorkforce/burn#245.
//!
//! 1. Read a fixture written in the exact byte shape `@relayburn/ingest`'s
//!    TS adapter produces (`JSON.stringify(record, null, 2) + '\n'`, key
//!    order matching `writePendingStamp`'s object literal). Confirm the
//!    Rust adapter parses and re-serializes it byte-identically.
//! 2. Write a stamp from the Rust adapter and confirm the bytes round-trip
//!    through the same parser, end-to-end. The TS adapter's parser is
//!    explicit-key and accepts the same shape, so a Rust-written file
//!    drops onto a TS-resident watch loop without modification.

use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn rust_parses_ts_written_stamp() {
    let raw = std::fs::read_to_string(fixture_path("ts_codex_stamp.json")).unwrap();
    let parsed =
        relayburn_ingest::pending_stamps::parse_pending_stamp(&raw).expect("ts fixture parses");
    assert_eq!(parsed.v, 1);
    assert_eq!(parsed.harness, relayburn_ingest::PendingStampHarness::Codex);
    assert_eq!(parsed.spawner_pid, 12345);
    assert_eq!(parsed.spawn_start_ts, "2025-05-01T12:34:56.789Z");
    assert_eq!(parsed.cwd, "/home/user/repo");
    assert_eq!(
        parsed.session_dir_hint.as_deref(),
        Some("/home/user/repo/.codex")
    );
    assert_eq!(parsed.enrichment.get("role").unwrap(), "fix-bug");
    assert_eq!(parsed.enrichment.get("ticket").unwrap(), "CRD-42");
}

#[test]
fn rust_reserialization_is_byte_identical_to_ts_fixture() {
    let raw = std::fs::read_to_string(fixture_path("ts_codex_stamp.json")).unwrap();
    let parsed = relayburn_ingest::pending_stamps::parse_pending_stamp(&raw).unwrap();
    let reserialized = relayburn_ingest::pending_stamps::serialize_stamp(&parsed);
    assert_eq!(
        reserialized, raw,
        "Rust re-serialization diverged from TS-written fixture"
    );
}

#[test]
fn rust_written_stamp_round_trips_through_parser() {
    use relayburn_ingest::pending_stamps::{parse_pending_stamp, serialize_stamp};
    use relayburn_ingest::{PendingStamp, PendingStampHarness};

    let mut enrichment = std::collections::BTreeMap::new();
    enrichment.insert("role".into(), "ship".into());
    enrichment.insert("branch".into(), "main".into());
    let original = PendingStamp {
        v: 1,
        harness: PendingStampHarness::Opencode,
        spawner_pid: 99,
        spawn_start_ts: "2026-01-15T08:09:10.011Z".into(),
        cwd: "/var/tmp/work".into(),
        enrichment,
        session_dir_hint: Some("/var/tmp/work/.opencode".into()),
    };
    let serialized = serialize_stamp(&original);
    let reparsed = parse_pending_stamp(&serialized).unwrap();
    assert_eq!(reparsed, original);
    // Trailing newline: matches `JSON.stringify(...) + '\n'` from the TS adapter.
    assert!(
        serialized.ends_with("\n"),
        "TS wire format requires trailing newline"
    );
}

#[test]
fn rejects_wrong_version() {
    let raw = r#"{"v":2,"harness":"codex","spawnerPid":1,"spawnStartTs":"2025-05-01T00:00:00.000Z","cwd":"/x","enrichment":{}}"#;
    assert!(
        relayburn_ingest::pending_stamps::parse_pending_stamp(raw).is_none(),
        "bumped version must trip the parser so a forward-incompatible writer can't poison the matcher"
    );
}

#[test]
fn rejects_unknown_harness() {
    let raw = r#"{"v":1,"harness":"cursor","spawnerPid":1,"spawnStartTs":"2025-05-01T00:00:00.000Z","cwd":"/x","enrichment":{}}"#;
    assert!(relayburn_ingest::pending_stamps::parse_pending_stamp(raw).is_none());
}
