//! One instruction file injected at different lengths by different
//! harnesses (Codex's 32 KiB project budget vs OpenCode's full file) is two
//! overhead rows; the human report and trim output keep them apart.

use assert_cmd::Command;
use predicates::prelude::*;

const CODEX_PROJECT_BUDGET: usize = 32 * 1024;

/// `repo/AGENTS.md` consumes all but 200 bytes of Codex's budget, so Codex
/// injects only a prefix of `repo/sub/AGENTS.md` while OpenCode injects it
/// whole. HOME is sealed and `--kind agents-md` drops Claude's ancestor walk.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let repo = tmp.path().join("repo");
    let sub = repo.join("sub");
    std::fs::create_dir_all(repo.join(".git")).expect("git marker");
    std::fs::create_dir_all(&sub).expect("sub dir");
    std::fs::write(
        repo.join("AGENTS.md"),
        "a".repeat(CODEX_PROJECT_BUDGET - 200),
    )
    .expect("root AGENTS.md");
    let section = |name: &str| format!("## {name}\n\n{}\n\n", "guidance words ".repeat(12));
    std::fs::write(
        sub.join("AGENTS.md"),
        [section("First"), section("Second"), section("Third")].concat(),
    )
    .expect("sub AGENTS.md");
    (tmp, sub)
}

fn burn(tmp: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("burn").expect("burn binary");
    cmd.env("HOME", tmp.path().join("home"))
        .env("RELAYBURN_HOME", tmp.path().join("ledger"))
        .env_remove("CODEX_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("NO_COLOR", "1");
    cmd
}

#[test]
fn report_renders_each_row_with_its_own_metadata() {
    let (tmp, sub) = fixture();
    let output = burn(&tmp)
        .args(["overhead", "--kind", "agents-md", "--project"])
        .arg(&sub)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf-8");
    let sub_rows: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("sub/AGENTS.md"))
        .collect();
    assert_eq!(sub_rows.len(), 2, "{stdout}");
    assert!(sub_rows[0].ends_with("applies to: codex"), "{stdout}");
    assert!(sub_rows[1].ends_with("applies to: opencode"), "{stdout}");
    assert_ne!(
        sub_rows[0].split(" — ").nth(1),
        sub_rows[1].split(" — ").nth(1),
        "the Codex prefix and the full file differ in size: {stdout}"
    );
}

#[test]
fn trim_groups_recommendations_per_row() {
    let (tmp, sub) = fixture();
    burn(&tmp)
        .args(["overhead", "trim", "--kind", "agents-md", "--project"])
        .arg(&sub)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "# === AGENTS.md (applies to: codex) ===",
        ))
        .stdout(predicate::str::contains(
            "# === AGENTS.md (applies to: opencode) ===",
        ));
}
