//! Smoke test for the `burn` CLI scaffold.
//!
//! Drives the actual binary (`cargo run -p relayburn-cli --bin burn`)
//! through `assert_cmd` to prove that:
//!
//! 1. `burn --help` exits 0 and emits non-empty stdout listing all
//!    eight subcommands (the contract Wave 2 fan-out PRs depend on).
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
    "run",
    "state",
    "ingest",
    "mcp-server",
];

/// Subcommands that still print "not yet implemented" when invoked
/// without args. Wave 2 D1 wired up `summary` and `hotspots`, D2 wired
/// up `overhead`, D3 wired up `compare`, D4 wired up `state`, and D5
/// wired up `run` as real presenters, so they're excluded from the
/// stub-mode tripwire below. The remaining entries are owned by sibling
/// Wave 2 PRs. As each Wave 2 D1–D8 PR wires its presenter, drop the
/// command from this list — the missing entries fall under a more
/// targeted assertion (see `compare_command_rejects_missing_models` and
/// `run_command_rejects_unknown_harness` below for examples).
const UNIMPLEMENTED_SUBCOMMANDS: &[&str] = &[
    "ingest",
    "mcp-server",
];

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
fn json_mode_emits_error_envelope_on_unimplemented() {
    // The `--json` global flips error reporting from a stderr line to
    // a `{"error": …}` JSON envelope on stdout. Cover the toggle so
    // Wave 2 commands inherit a consistent JSON-mode error shape.
    // Use a still-stubbed command (`ingest`) so the assertion remains
    // meaningful as Wave 2 PRs replace stubs with real presenters.
    let output = burn()
        .args(["--json", "ingest"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("\"error\""),
        "expected JSON-mode envelope on stdout; got:\n{stdout}",
    );
    assert!(
        stdout.contains("not yet implemented"),
        "expected JSON-mode envelope to carry the not-yet-implemented message; got:\n{stdout}",
    );
}

#[test]
fn run_command_lists_known_harnesses_when_invoked_without_args() {
    // `burn run` (Wave 2 D5) prints help + exits 2 when no harness
    // positional is supplied — the same shape as the TS sibling.
    let output = burn().arg("run").assert().code(2).get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("Known harnesses:"),
        "expected `burn run` to list known harnesses; got:\n{stdout}",
    );
    assert!(
        stdout.contains("claude"),
        "expected `burn run` help to mention claude; got:\n{stdout}",
    );
}

#[test]
fn run_command_rejects_unknown_harness() {
    // Unknown harness must exit non-zero with a typed error mentioning
    // both the bogus name and the known set. Driver maps this through
    // `report_error`, which lands at exit code 2 in human mode.
    let output = burn()
        .args(["run", "definitely-not-a-real-harness"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("definitely-not-a-real-harness"),
        "expected stderr to echo the unknown harness name; got:\n{stderr}",
    );
    assert!(
        stderr.contains("claude"),
        "expected stderr to list claude as a known harness; got:\n{stderr}",
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
