use super::{format_bytes, format_cutoff_ts, format_status, parse_retention, rel_to_home};
use relayburn_sdk::{Retention, StateStatus};
use serde_json::json;

/// Mirror of `relayburn_sdk::ledger::writer::now_lex_token`'s format string.
/// Re-deriving it locally guards against the writer's format drifting
/// without the cutoff helper following.
fn writer_style_ts(secs: u64, nanos_part: u64) -> String {
    format!("ts:{:020}.{:09}", secs, nanos_part)
}

#[test]
fn cutoff_matches_writer_format_byte_for_byte() {
    // 1234.567 seconds since epoch, expressed in ms, must produce
    // the same string the writer would stamp for that instant.
    let ms = 1_234_567u64;
    let writer = writer_style_ts(1_234, 567_000_000);
    assert_eq!(format_cutoff_ts(ms), writer);
}

#[test]
fn cutoff_is_lex_comparable_against_writer_rows() {
    // A row stamped *before* the cutoff sorts lex-less; a row
    // stamped *after* sorts lex-greater. This is the invariant
    // `prune_content_older_than(&cutoff)` relies on.
    let cutoff = format_cutoff_ts(2_000); // 2.000s
    let earlier_row = writer_style_ts(1, 500_000_000); // 1.500s
    let later_row = writer_style_ts(2, 500_000_000); // 2.500s
    assert!(earlier_row.as_str() < cutoff.as_str());
    assert!(later_row.as_str() > cutoff.as_str());
}

#[test]
fn cutoff_padding_widths_are_stable() {
    // Width of `ts:` + 20-digit secs + `.` + 9-digit nanos = 33.
    // A narrower padding (the original `{:013}.000` bug) would flip
    // the lex ordering — cover the constant here so a formatting
    // tweak that breaks the invariant fails this test first.
    assert_eq!(format_cutoff_ts(0).len(), 33);
    assert_eq!(format_cutoff_ts(u64::MAX).len(), 33);
}

#[test]
fn rel_to_home_rewrites_paths_inside_home() {
    assert_eq!(
        rel_to_home("/x/home/burn.sqlite", "/x/home"),
        "${RELAYBURN_HOME}/burn.sqlite"
    );
    assert_eq!(
        rel_to_home("/x/home/sub/dir/file", "/x/home"),
        "${RELAYBURN_HOME}/sub/dir/file"
    );
}

#[test]
fn rel_to_home_rejects_byte_prefix_siblings() {
    // The bug guarded against: `/x/home2/...` mustn't be treated as
    // `home="/x/home"`'s child (would have rewritten to
    // `${RELAYBURN_HOME}/2/...` under the old `starts_with` byte
    // match).
    assert_eq!(
        rel_to_home("/x/home2/burn.sqlite", "/x/home"),
        "/x/home2/burn.sqlite"
    );
    assert_eq!(rel_to_home("/x/homer", "/x/home"), "/x/homer");
}

#[test]
fn rel_to_home_normalizes_trailing_slash_on_home() {
    // `home` with or without a trailing slash should produce the
    // same rewrite for paths underneath.
    assert_eq!(
        rel_to_home("/x/home/burn.sqlite", "/x/home/"),
        "${RELAYBURN_HOME}/burn.sqlite"
    );
    assert_eq!(rel_to_home("/x/home2/foo", "/x/home/"), "/x/home2/foo");
}

#[test]
fn rel_to_home_handles_degenerate_home_inputs() {
    // Empty home is a passthrough; a `/`-only home is too — the
    // rewrite would be meaningless ("everything is inside root").
    assert_eq!(rel_to_home("/x/home/foo", ""), "/x/home/foo");
    assert_eq!(rel_to_home("/x/home/foo", "/"), "/x/home/foo");
    assert_eq!(rel_to_home("/x/home/foo", "//"), "/x/home/foo");
}

#[test]
fn rel_to_home_path_equals_home() {
    // `path == home` preserves the prior trailing-slash output
    // shape (`${RELAYBURN_HOME}/`) — callers downstream that
    // pattern-match on the prefix shouldn't see a behavioral
    // change here.
    assert_eq!(rel_to_home("/x/home", "/x/home"), "${RELAYBURN_HOME}/");
    assert_eq!(rel_to_home("/x/home/", "/x/home"), "${RELAYBURN_HOME}/");
}

#[test]
fn format_bytes_small_values_stay_in_bytes() {
    assert_eq!(format_bytes(0), "0 bytes");
    assert_eq!(format_bytes(1023), "1023 bytes");
}

#[test]
fn format_bytes_picks_unit_and_precision() {
    assert_eq!(format_bytes(1024), "1.00 KB");
    assert_eq!(format_bytes(1536), "1.50 KB");
    assert_eq!(format_bytes(10 * 1024), "10.0 KB");
    assert_eq!(format_bytes(100 * 1024), "100 KB");
    assert_eq!(format_bytes(1023 * 1024), "1023 KB");
    assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
    assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GB");
    assert_eq!(format_bytes(2 * 1024u64.pow(4)), "2.00 TB");
}

#[test]
fn format_bytes_caps_at_terabytes() {
    assert_eq!(format_bytes(2048 * 1024u64.pow(4)), "2048 TB");
}

#[test]
fn parse_retention_rejects_empty_and_garbage() {
    assert_eq!(parse_retention(""), None);
    assert_eq!(parse_retention("   "), None);
    assert_eq!(parse_retention("ten"), None);
    assert_eq!(parse_retention("NaN"), None);
    assert_eq!(parse_retention("inf"), None);
}

#[test]
fn parse_retention_forever_is_case_insensitive_and_trimmed() {
    assert_eq!(parse_retention("forever"), Some(Retention::Forever));
    assert_eq!(parse_retention("  FOREVER "), Some(Retention::Forever));
}

#[test]
fn parse_retention_negative_means_forever() {
    assert_eq!(parse_retention("-1"), Some(Retention::Forever));
    assert_eq!(parse_retention("-0.5"), Some(Retention::Forever));
}

#[test]
fn parse_retention_accepts_numeric_days() {
    assert_eq!(parse_retention("0"), Some(Retention::Days(0.0)));
    assert_eq!(parse_retention(" 30 "), Some(Retention::Days(30.0)));
    assert_eq!(parse_retention("0.5"), Some(Retention::Days(0.5)));
}

fn status_fixture(
    burn_exists: bool,
    content_exists: bool,
    archive: serde_json::Value,
    config: serde_json::Value,
) -> StateStatus {
    serde_json::from_value(json!({
        "home": "/x/home",
        "burn": {
            "path": "/x/home/burn.sqlite",
            "exists": burn_exists,
            "rows": {
                "turns": 1234,
                "userTurns": 2,
                "compactions": 3,
                "relationships": 4,
                "toolResultEvents": 5,
                "inferences": 6,
                "sessions": 7,
                "stamps": 8
            },
            "trackedRows": 1269
        },
        "content": {
            "path": "/elsewhere/content.sqlite",
            "exists": content_exists,
            "rows": 4321
        },
        "archive": archive,
        "config": config
    }))
    .expect("valid StateStatus fixture")
}

#[test]
fn format_status_renders_every_field() {
    let s = status_fixture(
        true,
        true,
        json!({
            "schemaVersion": 5,
            "lastBuiltAt": "2026-01-01T00:00:00.000Z",
            "lastRebuildAt": "2026-02-01T00:00:00.000Z",
            "lastWriteAtMs": 0
        }),
        json!({ "store": "sqlite", "retentionDays": 30.0, "retentionForever": false }),
    );
    let expected = "\
derived state at /x/home:
events DB (burn.sqlite):
  path: ${RELAYBURN_HOME}/burn.sqlite
  tracked rows: 1,269
    turns:              1,234
    user_turns:         2
    compactions:        3
    relationships:      4
    tool_result_events: 5
    inferences:         6
    sessions:           7
    stamps:             8
content DB (content.sqlite):
  path: /elsewhere/content.sqlite
  rows: 4,321
archive state:
  schema version: 5
  last built:   2026-01-01T00:00:00.000Z
  last rebuild: 2026-02-01T00:00:00.000Z
  last write:   1970-01-01T00:00:00.000Z
config:
  store: sqlite
  retention: 30 days
";
    assert_eq!(format_status(&s), expected);
}

#[test]
fn format_status_marks_missing_dbs_and_never_timestamps() {
    let s = status_fixture(
        false,
        false,
        json!({ "schemaVersion": 1 }),
        json!({ "store": "sqlite", "retentionDays": 0.5, "retentionForever": false }),
    );
    let out = format_status(&s);
    assert_eq!(out.matches("  status: not built yet\n").count(), 2);
    assert!(out.contains("events DB (burn.sqlite):\n  path: ${RELAYBURN_HOME}/burn.sqlite\n  status: not built yet\n  tracked rows:"));
    assert!(out.contains("content DB (content.sqlite):\n  path: /elsewhere/content.sqlite\n  status: not built yet\n  rows:"));
    assert!(out.contains("  last built:   never\n"));
    assert!(out.contains("  last rebuild: never\n"));
    assert!(out.contains("  last write:   never\n"));
    assert!(out.ends_with("  retention: 0.5 days\n"), "{out}");
}

#[test]
fn format_status_retention_forever_variants() {
    let forever_flag = status_fixture(
        true,
        true,
        json!({ "schemaVersion": 1 }),
        json!({ "store": "sqlite", "retentionDays": 30.0, "retentionForever": true }),
    );
    assert!(format_status(&forever_flag).ends_with("  retention: forever\n"));

    let no_days = status_fixture(
        true,
        true,
        json!({ "schemaVersion": 1 }),
        json!({ "store": "sqlite", "retentionForever": false }),
    );
    let out = format_status(&no_days);
    assert!(out.ends_with("  retention: forever\n"), "{out}");
    assert!(!out.contains("not built yet"));
}
