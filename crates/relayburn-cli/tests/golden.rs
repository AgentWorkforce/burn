//! TS-CLI vs Rust-CLI golden-output diff runner.
//!
//! For each invocation listed in `tests/fixtures/cli-golden/invocations.json`,
//! this test:
//!   1. Spawns the Rust `burn` binary against the fixture ledger and project,
//!      with the same sealed env the TS capture used (`HOME` pointed at an
//!      empty tmp dir, `RELAYBURN_HOME` at the fixture, `RELAYBURN_ARCHIVE=0`,
//!      `NO_COLOR=1`).
//!   2. Reads the captured TS stdout snapshot (and stderr if present).
//!   3. Normalizes the live Rust output the same way the capture script does
//!      (absolute fixture paths → `${RELAYBURN_HOME}` / `${PROJECT}`,
//!      wall-clock millisecond fields → `${MTIME}` / `${TS}`).
//!   4. Asserts the normalized Rust output matches the snapshot byte-for-byte
//!      and prints a unified diff on mismatch.
//!
//! ## Why this is `#[ignore]`d on `main`
//!
//! Today the Rust CLI is a `eprintln!("not yet implemented") + exit(1)` stub
//! — every snapshot will fail. That's deliberate: this PR (#248-c) ships the
//! *target* the Wave 2 fan-out PRs (#248 D1–D8 in `RUST_PORT_WAVE_PLAN.md`)
//! get to assert against. As each command lands its Rust implementation,
//! the matching invocation in `invocations.json` flips its `enabled` flag
//! to `true` and the test starts enforcing parity.
//!
//! Run the full enforced suite locally with:
//!   BURN_GOLDEN=1 cargo test --test golden -- --include-ignored
//!
//! Refresh the TS snapshots after a CLI behavior change with:
//!   pnpm run build && \
//!   node tests/fixtures/cli-golden/scripts/capture-snapshots.mjs
//!
//! See `tests/fixtures/cli-golden/README.md` for the full Wave 2 contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use relayburn_sdk::{
    CompactionEvent, RawLedger, SessionRelationshipRecord, Stamp, ToolResultEventRecord,
    TurnRecord, UserTurnRecord,
};
use serde::Deserialize;

/// Bootstrap the SQLite fixture from the committed `ledger.jsonl`.
///
/// The CLI-golden fixture's source of truth is `ledger.jsonl` (the SQLite
/// counterparts are gitignored because they're rematerialized on demand;
/// see `tests/fixtures/cli-golden/ledger/.gitignore`). The TS CLI reads
/// JSONL natively via its `file` storage adapter; the Rust SDK is sqlite-
/// only, so we replay the JSONL into `burn.sqlite` here before invoking
/// the binary.
///
/// Idempotent: if `burn.sqlite` already has a non-empty `turns` table we
/// skip the rebuild. Local devs running the diff runner repeatedly thus
/// only pay the JSONL-replay cost once.
fn bootstrap_sqlite_from_jsonl(ledger_home: &Path) -> std::io::Result<()> {
    let jsonl_path = ledger_home.join("ledger.jsonl");
    if !jsonl_path.is_file() {
        // No JSONL source: assume the fixture was built another way and
        // bail (the binary will surface the resulting empty-ledger
        // diff loud and clear when it runs).
        return Ok(());
    }
    let burn_path = ledger_home.join("burn.sqlite");
    let content_path = ledger_home.join("content.sqlite");

    // Wipe any prior bootstrap. We don't try to do incremental upserts
    // here — the JSONL is canonical and small; rewriting from scratch
    // keeps the bootstrap deterministic across runs.
    for name in [
        "burn.sqlite",
        "burn.sqlite-shm",
        "burn.sqlite-wal",
        "content.sqlite",
        "content.sqlite-shm",
        "content.sqlite-wal",
    ] {
        let p = ledger_home.join(name);
        let _ = fs::remove_file(p);
    }

    let mut ledger = RawLedger::open(&burn_path, &content_path)
        .expect("open fixture ledger for bootstrap");

    let raw = fs::read_to_string(&jsonl_path)?;
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut user_turns: Vec<UserTurnRecord> = Vec::new();
    let mut tool_results: Vec<ToolResultEventRecord> = Vec::new();
    let mut relationships: Vec<SessionRelationshipRecord> = Vec::new();
    let mut compactions: Vec<CompactionEvent> = Vec::new();
    let mut stamps: Vec<Stamp> = Vec::new();

    for (line_no, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let envelope: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|err| {
            panic!(
                "[golden bootstrap] line {} of ledger.jsonl is not valid JSON: {err}",
                line_no + 1
            )
        });
        let kind = envelope
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut record = envelope.get("record").cloned().unwrap_or(serde_json::Value::Null);
        match kind {
            "turn" => turns.push(serde_json::from_value(record).expect("turn record")),
            "user_turn" => {
                user_turns.push(serde_json::from_value(record).expect("user_turn record"))
            }
            "tool_result_event" => {
                normalize_tool_result_event(&mut record);
                tool_results
                    .push(serde_json::from_value(record).expect("tool_result_event record"))
            }
            "relationship" => relationships
                .push(serde_json::from_value(record).expect("relationship record")),
            "compaction" => {
                compactions.push(serde_json::from_value(record).expect("compaction record"))
            }
            "stamp" => stamps.push(stamp_from_envelope(&envelope)),
            _ => {
                // Unknown kinds (`text`, `tool_result`, etc. emitted by older
                // ledger writers) are noise here — they belong to the content
                // sidecar lifecycle, not the events DB.
            }
        }
    }

    ledger.append_turns(&turns).expect("append turns");
    ledger
        .append_user_turns(&user_turns)
        .expect("append user_turns");
    ledger
        .append_tool_result_events(&tool_results)
        .expect("append tool_result_events");
    ledger
        .append_relationships(&relationships)
        .expect("append relationships");
    ledger
        .append_compactions(&compactions)
        .expect("append compactions");
    for s in &stamps {
        ledger.append_stamp(s).expect("append stamp");
    }
    Ok(())
}

/// The hand-built fixture writes `eventSource: "transcript"` for Claude
/// `tool_result` events; the canonical schema dropped that variant in
/// favor of the more specific `"tool_result"` value. The TS reader is
/// lenient and stores the JSON verbatim; the Rust SDK is strict. Normalize
/// here so the fixture replays cleanly without retroactively rewriting
/// the JSONL on disk (which would drift the snapshot capture corpus). The
/// substitution also fills in `eventIndex` if the fixture omits it
/// (required by the SDK schema; the TS reader defaults missing values to
/// `0` via `??`).
fn normalize_tool_result_event(record: &mut serde_json::Value) {
    let Some(obj) = record.as_object_mut() else {
        return;
    };
    if let Some(src) = obj.get_mut("eventSource") {
        if src.as_str() == Some("transcript") {
            *src = serde_json::Value::String("tool_result".to_string());
        }
    }
    obj.entry("eventIndex").or_insert(serde_json::json!(0));
}

fn stamp_from_envelope(envelope: &serde_json::Value) -> Stamp {
    Stamp {
        ts: envelope
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        selector: serde_json::from_value(
            envelope
                .get("selector")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default(),
        enrichment: serde_json::from_value(
            envelope
                .get("enrichment")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invocation {
    name: String,
    args: Vec<String>,
    #[serde(default)]
    expect_status: Option<i32>,
    /// Set to true once the Rust CLI implements the command surface this
    /// snapshot covers. Wave 2 PRs flip this per-command. Until then the
    /// test for that invocation is skipped *unconditionally* (the diff
    /// runner reports "skipped: not yet enabled" rather than failing).
    #[serde(default)]
    enabled: bool,
    /// Optional extra env to set for this specific invocation. Mirrors
    /// `inv.env` in the JSON contract so capture-snapshots.mjs and
    /// golden.rs stay aligned.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[test]
fn golden_diff_against_ts_cli_snapshots() {
    if std::env::var("BURN_GOLDEN").ok().as_deref() != Some("1") {
        // CI runs `cargo test --workspace` without BURN_GOLDEN set, so the
        // diff runner is silent there. Local devs run `BURN_GOLDEN=1
        // cargo test --test golden -- --nocapture` to enforce the gate;
        // once Wave 2 finishes, the gate flips on by default in CI.
        // Return early so an unset BURN_GOLDEN truly skips — no fixture
        // discovery, no snapshot reads, no env-prep work.
        eprintln!(
            "[golden] BURN_GOLDEN!=1 — skipping (set BURN_GOLDEN=1 to enforce). \
             Even when enforced, individual invocations stay skipped until their \
             `enabled: true` flag is set in invocations.json."
        );
        return;
    }

    let fixture_dir = repo_root().join("tests").join("fixtures").join("cli-golden");
    assert!(
        fixture_dir.is_dir(),
        "fixture corpus missing at {}",
        fixture_dir.display()
    );

    let invocations_path = fixture_dir.join("invocations.json");
    let raw = fs::read_to_string(&invocations_path).unwrap_or_else(|err| {
        panic!(
            "failed to read invocations from {}: {err}",
            invocations_path.display()
        )
    });
    let invocations: Vec<Invocation> = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("invocations.json is malformed: {err}"));

    let snapshots_dir = fixture_dir.join("snapshots");
    let ledger_home = fixture_dir.join("ledger");
    let project_dir = fixture_dir.join("project");

    // Bootstrap the SQLite fixture from `ledger.jsonl`. The Rust SDK only
    // reads from sqlite; the in-tree fixture is JSONL-only because the
    // sqlite binaries are gitignored. Replays once per test run.
    bootstrap_sqlite_from_jsonl(&ledger_home).expect("bootstrap sqlite from JSONL");

    // Sealed HOME so the Rust binary's eventual ingest sweep doesn't
    // discover the developer's real session stores.
    let sealed_home = tempdir_under(&fixture_dir);

    let burn = burn_binary_path();

    let mut failures = Vec::new();
    for inv in &invocations {
        if !inv.enabled {
            eprintln!("[golden] skip {} (enabled=false)", inv.name);
            continue;
        }
        // The whole-test BURN_GOLDEN!=1 short-circuit at the top returned
        // before this loop, so by the time we get here the gate is set.

        let snapshot_stdout = snapshots_dir.join(format!("{}.stdout.txt", inv.name));
        let expected_stdout = fs::read_to_string(&snapshot_stdout).unwrap_or_else(|err| {
            panic!(
                "snapshot missing for {} ({}): {err}",
                inv.name,
                snapshot_stdout.display()
            )
        });
        let snapshot_stderr = snapshots_dir.join(format!("{}.stderr.txt", inv.name));
        let expected_stderr = if snapshot_stderr.is_file() {
            fs::read_to_string(&snapshot_stderr).unwrap_or_default()
        } else {
            String::new()
        };

        let mut cmd = Command::new(&burn);
        cmd.args(&inv.args)
            .current_dir(repo_root())
            .env_clear()
            // Keep PATH so the binary can find shared libraries; everything
            // else gets a sealed value.
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &sealed_home)
            .env("RELAYBURN_HOME", &ledger_home)
            .env("RELAYBURN_CONTENT_STORE", "off")
            .env("RELAYBURN_ARCHIVE", "0")
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0");
        for (k, v) in &inv.env {
            cmd.env(k, v);
        }

        let output = match cmd.output() {
            Ok(o) => o,
            Err(err) => {
                failures.push(format!("{}: spawn failed: {err}", inv.name));
                continue;
            }
        };

        let expected_status = inv.expect_status.unwrap_or(0);
        let actual_status = output.status.code().unwrap_or(-1);
        let stdout = normalize(
            std::str::from_utf8(&output.stdout).unwrap_or(""),
            &ledger_home,
            &project_dir,
        );
        let stderr = normalize(
            std::str::from_utf8(&output.stderr).unwrap_or(""),
            &ledger_home,
            &project_dir,
        );

        let mut diffs = Vec::new();
        if actual_status != expected_status {
            diffs.push(format!(
                "  exit status: expected {expected_status}, got {actual_status}"
            ));
        }
        if stdout != expected_stdout {
            diffs.push(format!(
                "  stdout mismatch:\n{}",
                indent(&unified_diff(&expected_stdout, &stdout), "    "),
            ));
        }
        if stderr != expected_stderr {
            diffs.push(format!(
                "  stderr mismatch:\n{}",
                indent(&unified_diff(&expected_stderr, &stderr), "    "),
            ));
        }
        if !diffs.is_empty() {
            failures.push(format!("{}:\n{}", inv.name, diffs.join("\n")));
        } else {
            eprintln!("[golden] ok   {}", inv.name);
        }
    }

    let _ = fs::remove_dir_all(&sealed_home);

    if !failures.is_empty() {
        panic!(
            "{} golden diff failure(s):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `.../crates/relayburn-cli`. Walk up two levels
    // to land at the workspace root regardless of which worktree we're in.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR has no two-levels-up parent")
}

fn burn_binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests on the
    // crate that owns the [[bin]]. Falls back to a workspace-relative path
    // if a developer runs the test outside cargo (rare but possible).
    if let Some(p) = option_env!("CARGO_BIN_EXE_burn") {
        return PathBuf::from(p);
    }
    repo_root()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "burn.exe" } else { "burn" })
}

/// Apply the same path / mtime placeholders the capture script uses so the
/// snapshot stays portable across machines. Keep this in sync with
/// `tests/fixtures/cli-golden/scripts/capture-snapshots.mjs::normalize`.
///
/// The synthetic ledger embeds `/tmp/golden-project` as a fake project /
/// tool-target path; we substitute it here too so the Rust binary's output
/// matches the snapshot byte-for-byte regardless of how the path appears
/// on the host (it's a literal in the JSON, not a real filesystem path).
fn normalize(text: &str, ledger_home: &Path, project_dir: &Path) -> String {
    let mut out = text.replace(
        ledger_home.to_str().expect("ledger home is utf8"),
        "${RELAYBURN_HOME}",
    );
    out = out.replace(
        project_dir.to_str().expect("project dir is utf8"),
        "${PROJECT}",
    );
    out = out.replace("/tmp/golden-project", "${FIXTURE_PROJECT}");
    out = squash_numeric_field(&out, "ledgerMtimeMsCurrent", "${MTIME}");
    out = squash_numeric_field(&out, "lastBuiltAt", "${TS}");
    out = squash_numeric_field(&out, "lastRebuildAt", "${TS}");
    out
}

/// Replace `"<key>": <digits>` (with any whitespace after the colon) with
/// `"<key>": "<placeholder>"`. Mirrors the JS regex in normalize() in the
/// capture script.
fn squash_numeric_field(text: &str, key: &str, placeholder: &str) -> String {
    let needle = format!("\"{key}\":");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(&needle) {
        out.push_str(&rest[..idx]);
        out.push_str(&needle);
        let after_key = &rest[idx + needle.len()..];
        // Mirror the JS capture path's `\s*\d+` semantics. JS's `\s` matches
        // the full ASCII whitespace set (space, tab, LF, CR, VT, FF) plus
        // some Unicode spaces; JSON is ASCII at this layer so the byte set
        // below is the right scope. NB: `char::is_ascii_whitespace` is *not*
        // equivalent — it excludes U+000B (vertical tab), which JS `\s` does
        // match, so we list the bytes explicitly.
        let trimmed_start = after_key.trim_start_matches(|c: char| {
            matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
        });
        let ws_consumed = after_key.len() - trimmed_start.len();
        // If the value isn't a bare integer (e.g. `null`), bail and emit
        // the original bytes untouched.
        let digits_end = trimmed_start
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed_start.len());
        if digits_end == 0 {
            out.push_str(&after_key[..ws_consumed]);
            rest = &after_key[ws_consumed..];
            continue;
        }
        out.push(' ');
        out.push('"');
        out.push_str(placeholder);
        out.push('"');
        rest = &trimmed_start[digits_end..];
    }
    out.push_str(rest);
    out
}

fn unified_diff(expected: &str, actual: &str) -> String {
    // Hand-rolled minimal LCS-free diff: walk both side-by-side and emit
    // `-`/`+` markers wherever a line differs. This is intentionally not a
    // full Myers diff — it's enough to make a per-line drift obvious in
    // the panic message without dragging in a `similar` dependency for
    // a stub test.
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    let max = exp_lines.len().max(act_lines.len());
    let mut out = String::new();
    for i in 0..max {
        let e = exp_lines.get(i).copied();
        let a = act_lines.get(i).copied();
        match (e, a) {
            (Some(e), Some(a)) if e == a => {
                out.push_str("  ");
                out.push_str(e);
                out.push('\n');
            }
            (Some(e), Some(a)) => {
                out.push_str("- ");
                out.push_str(e);
                out.push('\n');
                out.push_str("+ ");
                out.push_str(a);
                out.push('\n');
            }
            (Some(e), None) => {
                out.push_str("- ");
                out.push_str(e);
                out.push('\n');
            }
            (None, Some(a)) => {
                out.push_str("+ ");
                out.push_str(a);
                out.push('\n');
            }
            (None, None) => break,
        }
    }
    out
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tempdir_under(parent: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = parent.join(format!(".golden-home-{pid}-{nanos}"));
    fs::create_dir_all(&dir).expect("create sealed HOME");
    dir
}

#[cfg(test)]
mod tests {
    use super::squash_numeric_field;

    #[test]
    fn squash_numeric_field_matches_space_and_tab() {
        let input = "{\"lastBuiltAt\": 12345,\"lastRebuildAt\":\t67890}";
        let out = squash_numeric_field(input, "lastBuiltAt", "${TS}");
        let out = squash_numeric_field(&out, "lastRebuildAt", "${TS}");
        assert_eq!(
            out,
            "{\"lastBuiltAt\": \"${TS}\",\"lastRebuildAt\": \"${TS}\"}"
        );
    }

    #[test]
    fn squash_numeric_field_matches_newline_and_indent() {
        // Matches the JS regex `\s*\d+` semantics — if a formatter ever
        // pretty-prints a numeric field across a line break, the runner
        // still has to normalize it.
        let input = "{\"lastBuiltAt\":\n  12345}";
        let out = squash_numeric_field(input, "lastBuiltAt", "${TS}");
        assert_eq!(out, "{\"lastBuiltAt\": \"${TS}\"}");
    }

    #[test]
    fn squash_numeric_field_matches_carriage_return_and_other_ws() {
        // CR, vertical tab, form feed — all in `\s` and all ASCII whitespace.
        let input = "{\"lastBuiltAt\":\r\n\x0b\x0c 12345}";
        let out = squash_numeric_field(input, "lastBuiltAt", "${TS}");
        assert_eq!(out, "{\"lastBuiltAt\": \"${TS}\"}");
    }

    #[test]
    fn squash_numeric_field_leaves_non_numeric_value_untouched() {
        let input = r#"{"lastBuiltAt": null}"#;
        let out = squash_numeric_field(input, "lastBuiltAt", "${TS}");
        assert_eq!(out, input);
    }
}
