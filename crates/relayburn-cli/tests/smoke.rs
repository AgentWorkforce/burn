//! Smoke test for the `burn` CLI scaffold.
//!
//! Drives the actual binary (`cargo run -p relayburn-cli --bin burn`)
//! through `assert_cmd` to prove that:
//!
//! 1. `burn --help` exits 0 and emits non-empty stdout listing all
//!    registered subcommands.
//! 2. `burn <subcommand> --help` exits 0 for every subcommand we have a
//!    stub for. clap auto-generates the help block from the `Command`
//!    enum's doc comments, so a regression in the derive layer would
//!    surface here.
//! 3. Invoking a stub without `--help` exits 1 with the documented
//!    "not yet implemented" message — Wave 2 PRs replace this exit
//!    with their real presenter, so the test serves as a tripwire
//!    against an accidentally-empty stub.
//! 4. `burn --version` exits 0 (clap derives this from the workspace
//!    `package.version`).

use assert_cmd::Command;
use predicates::prelude::*;

/// Every top-level subcommand the scaffold registers. Keep this list
/// in sync with `cli::Command` — adding a variant there should bump
/// this list, and Wave 2 PRs that delete a stub should drop the entry
/// here as part of the same PR.
const SUBCOMMANDS: &[&str] = &[
    "summary",
    "hotspots",
    "overhead",
    "compare",
    "state",
    "sessions",
    "ingest",
    "mcp-server",
];

/// Subcommands that still print "not yet implemented" when invoked
/// without args. Wave 2 D1 wired up `summary` and `hotspots`, D2 wired
/// up `overhead`, D3 wired up `compare`, D4 wired up `state`, and D8 wired
/// up `ingest` + `mcp-server` as real
/// presenters — every subcommand is now wired, so this list is empty
/// and `each_stub_exits_one_with_not_yet_implemented_message` becomes
/// a no-op iteration. The constant is retained so a future scaffold
/// (a new stub subcommand) has somewhere to land without re-introducing
/// the iteration helper.
const UNIMPLEMENTED_SUBCOMMANDS: &[&str] = &[];

/// Helper: build a `Command` driving the locally-built `burn` binary.
fn burn() -> Command {
    Command::cargo_bin("burn").expect("`burn` binary must build for the smoke test")
}

#[test]
fn top_level_help_lists_every_subcommand() {
    let output = burn().arg("--help").assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("help should be valid UTF-8");
    assert!(!stdout.is_empty(), "--help must emit non-empty stdout");
    for sub in SUBCOMMANDS {
        assert!(
            stdout.contains(sub),
            "expected `--help` to mention subcommand `{sub}`; got:\n{stdout}",
        );
    }
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("run ")),
        "`burn --help` must not advertise removed `run` command; got:\n{stdout}",
    );
}

#[test]
fn each_subcommand_help_exits_zero_with_non_empty_stdout() {
    for sub in SUBCOMMANDS {
        let output = burn()
            .args([sub, "--help"])
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8(output.stdout).expect("help should be valid UTF-8");
        assert!(
            !stdout.is_empty(),
            "`{sub} --help` should emit non-empty stdout; got empty",
        );
    }
}

#[test]
fn overhead_trim_help_exits_zero_with_non_empty_stdout() {
    // `burn overhead` is no longer in UNIMPLEMENTED_SUBCOMMANDS, so the
    // parent `each_subcommand_help_exits_zero_with_non_empty_stdout`
    // covers its top-level help. The nested `trim` subcommand has its
    // own `clap` derive though; cover it explicitly so a regression in
    // the nested-action help wiring doesn't slip past CI.
    let output = burn()
        .args(["overhead", "trim", "--help"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).expect("help should be valid UTF-8");
    assert!(
        !stdout.is_empty(),
        "`overhead trim --help` should emit non-empty stdout; got empty",
    );
}

#[test]
fn each_stub_exits_one_with_not_yet_implemented_message() {
    for sub in UNIMPLEMENTED_SUBCOMMANDS {
        // Run the stub with no extra args. The default exit-code
        // contract for the scaffold is `EXIT_NOT_YET_IMPLEMENTED == 1`;
        // assert it explicitly so a future Wave 2 PR that wires up a
        // real presenter is forced to update this assertion (and the
        // scaffold acceptance criterion). Subcommands that have already
        // been wired up live in `SUBCOMMANDS` but not here.
        burn()
            .arg(sub)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("not yet implemented"));
    }
}

#[test]
fn compare_command_rejects_missing_models() {
    // `burn compare` is wired (Wave 2 D3); no positional list means
    // exit 2 + the canonical "needs at least 2 models" message. This
    // asserts the wired path exists so a future regression that nukes
    // the dispatch arm fails loud.
    burn()
        .arg("compare")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs at least 2 models"));
}

#[test]
fn json_mode_emits_error_envelope_on_argument_failure() {
    // The `--json` global flips error reporting from a stderr line to
    // a `{"error": …}` JSON envelope on stdout. Cover the toggle so
    // every wired Wave 2 command inherits a consistent JSON-mode error
    // shape. With every subcommand now wired, we pivot from the old
    // "still-stubbed" target to a wired command's argument-validation
    // failure (`burn compare` with no positional models) — same code
    // path through `report_error`, same envelope shape.
    let output = burn()
        .args(["--json", "compare"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("\"error\""),
        "expected JSON-mode envelope on stdout; got:\n{stdout}",
    );
    assert!(
        stdout.contains("needs at least 2 models"),
        "expected JSON-mode envelope to carry the compare error message; got:\n{stdout}",
    );
}

#[test]
fn version_flag_exits_zero() {
    burn()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn unknown_subcommand_exits_non_zero() {
    burn()
        .arg("definitely-not-a-real-subcommand")
        .assert()
        .failure();
}

#[test]
fn run_subcommand_is_not_registered() {
    burn().args(["run", "--help"]).assert().failure();
}

#[test]
fn hotspots_session_without_id_is_an_explicit_stub() {
    // `--session` with no value is the per-session aggregate / gap report
    // mode in the TS surface. The Rust port doesn't expose a relationship
    // / chronology query verb yet, so we exit 2 with a directed message
    // pointing users at the supported `--session <id>` filter. Cover the
    // tripwire so a future PR that lands the per-session view is forced
    // to update this assertion alongside the wiring.
    burn()
        .args(["hotspots", "--session"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "per-session aggregate view (`--session` with no id)",
        ));
}

#[test]
fn hotspots_explain_drift_is_an_explicit_stub() {
    burn()
        .args(["hotspots", "--explain-drift"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--explain-drift"));
}

#[test]
fn hotspots_unknown_pattern_value_is_rejected() {
    // `--patterns` accepts a CSV of detector kinds; an unknown kind is a
    // hard fail (exit 2) rather than a silent ignore.
    burn()
        .args(["hotspots", "--patterns", "definitely-not-a-detector"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown --patterns value"));
}

#[test]
fn hotspots_group_by_and_patterns_are_mutually_exclusive() {
    burn()
        .args(["hotspots", "--group-by", "file", "--patterns", "retry-loop"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("mutually exclusive"));
}

/// `burn state reset` (no `--force`) is a dry-run: it must open the
/// ledger, count what would be dropped, print a "would drop ... " line,
/// and exit 0 *without* mutating either DB. Pin the contract here so a
/// future refactor can't silently turn the dry-run destructive.
#[test]
fn state_reset_dry_run_does_not_mutate() {
    let home = tempfile::TempDir::new().expect("tmp RELAYBURN_HOME");

    burn()
        .args(["state", "reset"])
        .env("RELAYBURN_HOME", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run"))
        .stdout(predicate::str::contains("--force"));

    // Both DB files should exist (Ledger::open creates them) and be
    // sized like a freshly-bootstrapped empty layout.
    assert!(
        home.path().join("burn.sqlite").is_file(),
        "burn.sqlite must exist after dry-run open"
    );
    assert!(
        home.path().join("content.sqlite").is_file(),
        "content.sqlite must exist after dry-run open"
    );
}

/// `burn state reset --force` actually wipes; pair it with `--json` so
/// we can assert on the structured envelope without depending on the
/// human-readable format.
#[test]
fn state_reset_force_emits_executed_envelope() {
    let home = tempfile::TempDir::new().expect("tmp RELAYBURN_HOME");

    let output = burn()
        .args(["--json", "state", "reset", "--force"])
        .env("RELAYBURN_HOME", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json output is valid JSON");
    assert_eq!(value["executed"], serde_json::Value::Bool(true));
    assert_eq!(value["rowsDropped"], serde_json::Value::from(0));
    assert_eq!(value["stampsDropped"], serde_json::Value::from(0));
    assert_eq!(value["contentRowsDropped"], serde_json::Value::from(0));
    assert!(
        value.get("reingest").is_none(),
        "no `reingest` key without --reingest"
    );
}

/// `--reingest` requires `--force`. Clap should reject the lone flag at
/// parse time so a typo can't silently no-op.
#[test]
fn state_reset_reingest_requires_force() {
    burn()
        .args(["state", "reset", "--reingest"])
        .assert()
        .failure();
}

/// `burn sessions` is a parent verb that requires a nested subcommand;
/// invoking it bare should fail (clap's required-subcommand check) so a
/// future PR adding a sibling verb can't accidentally regress to a
/// silent no-op default.
#[test]
fn sessions_without_subcommand_fails() {
    burn().arg("sessions").assert().failure();
}

/// `burn sessions list` against an empty isolated ledger should open
/// cleanly, scan zero turns, and report "no sessions found" with exit 0.
/// Pins the empty-ledger path so a future regression in the SDK verb
/// can't silently start erroring on a fresh install.
#[test]
fn sessions_list_against_empty_ledger_reports_no_sessions() {
    let home = tempfile::TempDir::new().expect("tmp RELAYBURN_HOME");

    burn()
        .args(["sessions", "list"])
        .env("RELAYBURN_HOME", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions found"));
}

/// `burn sessions list --json` against an empty isolated ledger should
/// emit a valid JSON envelope with an empty `sessions` array and the
/// resolved filters echoed back. Asserting on `--json` separately from
/// the human form keeps the structured contract under test even if the
/// human format gets stylistically tweaked later.
#[test]
fn sessions_list_json_envelope_shape() {
    let home = tempfile::TempDir::new().expect("tmp RELAYBURN_HOME");

    let output = burn()
        .args(["--json", "sessions", "list", "--limit", "5"])
        .env("RELAYBURN_HOME", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json output is valid JSON");
    assert_eq!(value["limit"], serde_json::Value::from(5));
    assert_eq!(value["truncated"], serde_json::Value::Bool(false));
    assert_eq!(
        value["sessions"],
        serde_json::Value::Array(Vec::new()),
        "expected empty sessions array against fresh ledger"
    );
    assert_eq!(value["filters"]["since"], serde_json::Value::from("7d"));
}

/// `burn state rebuild classify` collapses onto the shared
/// `rebuild_derivable` transaction every other target uses. Against an
/// empty ledger this should open cleanly, drop zero rows, and exit 0;
/// `--json` carries the envelope shape so callers can structure-match
/// without depending on the human-readable form.
#[test]
fn state_rebuild_classify_emits_drop_envelope() {
    let home = tempfile::TempDir::new().expect("tmp RELAYBURN_HOME");

    let output = burn()
        .args(["--json", "state", "rebuild", "classify"])
        .env("RELAYBURN_HOME", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json output is valid JSON");
    assert_eq!(value["rowsDropped"], serde_json::Value::from(0));
    assert_eq!(value["contentRowsDropped"], serde_json::Value::from(0));
}
