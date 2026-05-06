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

/// Subcommands that have already been wired to a real presenter and
/// therefore no longer return the `not yet implemented` stub. Wave 2
/// PRs add their command name here as they land. Removed from the
/// not-yet-implemented assertion in
/// [`each_stub_exits_one_with_not_yet_implemented_message`].
const IMPLEMENTED_SUBCOMMANDS: &[&str] = &["overhead"];

/// Helper: build a `Command` driving the locally-built `burn` binary.
fn burn() -> Command {
    Command::cargo_bin("burn").expect("`burn` binary must build for the smoke test")
}

#[test]
fn top_level_help_lists_every_subcommand() {
    let output = burn()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();
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
fn each_stub_exits_one_with_not_yet_implemented_message() {
    for sub in SUBCOMMANDS {
        if IMPLEMENTED_SUBCOMMANDS.contains(sub) {
            continue;
        }
        // Run the stub with no extra args. The default exit-code
        // contract for the scaffold is `EXIT_NOT_YET_IMPLEMENTED == 1`;
        // assert it explicitly so a future Wave 2 PR that wires up a
        // real presenter is forced to update this assertion (and the
        // scaffold acceptance criterion).
        burn()
            .arg(sub)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("not yet implemented"));
    }
}

#[test]
fn json_mode_emits_error_envelope_on_unimplemented() {
    // The `--json` global flips error reporting from a stderr line to
    // a `{"error": …}` JSON envelope on stdout. Cover the toggle so
    // Wave 2 commands inherit a consistent JSON-mode error shape.
    let output = burn()
        .args(["--json", "summary"])
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
