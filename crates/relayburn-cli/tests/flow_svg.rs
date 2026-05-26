//! SVG-output snapshot test for `burn flow --output ...`.
//!
//! The cli-golden runner only diffs stdout/stderr, so the `--output`
//! path needs its own integration test. We:
//!
//! 1. Run the binary against the in-tree `cli-golden` fixture session.
//! 2. Write the SVG to a temp path.
//! 3. Compare the bytes against a checked-in snapshot under
//!    `tests/fixtures/flow-svg/flow-session-claude-3turn.svg`.
//!
//! The SVG output is deterministic for a fixed input (no timestamps,
//! no random ids) so byte-for-byte comparison is the right granularity.
//! Set `BURN_FLOW_SVG_REGEN=1` to overwrite the snapshot on the fly
//! when an intentional renderer change lands.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn flow_svg_matches_snapshot_for_claude_3turn_fixture() {
    let binary = burn_binary_path();
    if !binary.exists() {
        eprintln!(
            "[flow-svg] binary missing at {} — skipping (run `cargo build -p relayburn-cli` first)",
            binary.display()
        );
        return;
    }

    let workspace_root = repo_root();
    let ledger_home = workspace_root
        .join("tests")
        .join("fixtures")
        .join("cli-golden")
        .join("ledger");
    let snapshot_path = workspace_root
        .join("crates")
        .join("relayburn-cli")
        .join("tests")
        .join("fixtures")
        .join("flow-svg")
        .join("flow-session-claude-3turn.svg");

    let tmp_dir = tempdir_under(&workspace_root);
    let out_path = tmp_dir.join("flow.svg");

    let sealed_home = tempdir_under(&tmp_dir);

    let mut cmd = Command::new(&binary);
    cmd.args([
        "flow",
        "--session",
        "11111111-1111-1111-1111-111111111111",
        "--output",
    ])
    .arg(&out_path)
    .current_dir(&workspace_root)
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("HOME", &sealed_home)
    .env("RELAYBURN_HOME", &ledger_home)
    .env("RELAYBURN_CONTENT_STORE", "off")
    .env("RELAYBURN_ARCHIVE", "0")
    .env("NO_COLOR", "1")
    .env("FORCE_COLOR", "0");

    let status = cmd.status().expect("spawn `burn flow`");
    assert!(
        status.success(),
        "`burn flow ... --output` exited with {status}",
    );
    let actual_bytes = fs::read(&out_path).expect("read SVG output file");
    let actual = String::from_utf8(actual_bytes).expect("SVG is UTF-8");

    if std::env::var("BURN_FLOW_SVG_REGEN").ok().as_deref() == Some("1") {
        if let Some(parent) = snapshot_path.parent() {
            fs::create_dir_all(parent).expect("create snapshot dir");
        }
        fs::write(&snapshot_path, actual.as_bytes()).expect("write snapshot");
        let _ = fs::remove_dir_all(&tmp_dir);
        return;
    }

    let expected = fs::read_to_string(&snapshot_path).unwrap_or_else(|err| {
        panic!(
            "snapshot missing at {} ({err}); rerun with BURN_FLOW_SVG_REGEN=1 to create",
            snapshot_path.display()
        )
    });

    let _ = fs::remove_dir_all(&tmp_dir);

    assert_eq!(
        actual, expected,
        "SVG output drifted from snapshot at {}; rerun with BURN_FLOW_SVG_REGEN=1 if the change is intentional",
        snapshot_path.display()
    );
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR has no two-levels-up parent")
}

fn burn_binary_path() -> PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_burn") {
        return PathBuf::from(p);
    }
    repo_root()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "burn.exe" } else { "burn" })
}

fn tempdir_under(parent: &std::path::Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = parent.join(format!(".flow-svg-tmp-{pid}-{nanos}"));
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}
