//! `burn overhead` / `burn overhead trim` on a project with no active
//! instruction files: exit 1 with a scoped notice on stderr.

use assert_cmd::Command;
use predicates::prelude::*;

/// Sealed HOME and a git-bounded empty project, so discovery sees no user,
/// ancestor, or project instruction files.
fn sealed() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".git")).expect("create project");
    (tmp, project)
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
fn overhead_without_instruction_files_exits_one_with_notice() {
    let (tmp, project) = sealed();
    for args in [&["overhead"][..], &["overhead", "trim"][..]] {
        burn(&tmp)
            .args(args)
            .arg("--project")
            .arg(&project)
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("no overhead files found at"))
            .stderr(predicate::str::contains(
                "looked for active CLAUDE.md, CLAUDE.local.md, and AGENTS.md instruction chains",
            ));
    }
}

#[test]
fn overhead_kind_filter_names_the_missing_kind() {
    let (tmp, project) = sealed();
    burn(&tmp)
        .args(["overhead", "--kind", "agents-md", "--project"])
        .arg(&project)
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "no agents-md overhead files found at",
        ));
}
