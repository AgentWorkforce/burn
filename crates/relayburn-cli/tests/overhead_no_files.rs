//! `burn overhead` / `burn overhead trim` on a project with no active
//! instruction files: exit 1 with a scoped notice on stderr.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// Sealed HOME and a git-bounded empty project: no user or project
/// instruction files. Claude still walks ancestors to the filesystem root,
/// so only `--kind agents-md` is fully independent of the host.
fn sealed() -> (tempfile::TempDir, PathBuf) {
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

/// Host `CLAUDE.md` / `CLAUDE.local.md` files above the temp dir that
/// Claude's ancestor walk would legitimately report.
fn host_ancestor_claude_files(project: &Path) -> Vec<PathBuf> {
    project
        .ancestors()
        .flat_map(|dir| [dir.join("CLAUDE.md"), dir.join("CLAUDE.local.md")])
        .filter(|path| path.is_file())
        .collect()
}

#[test]
fn overhead_kind_filter_without_matches_exits_one_with_notice() {
    let (tmp, project) = sealed();
    for args in [&["overhead"][..], &["overhead", "trim"][..]] {
        burn(&tmp)
            .args(args)
            .args(["--kind", "agents-md", "--project"])
            .arg(&project)
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "no agents-md overhead files found at",
            ));
    }
}

#[test]
fn overhead_without_instruction_files_names_every_chain_it_checked() {
    let (tmp, project) = sealed();
    let host_files = host_ancestor_claude_files(&project);
    if !host_files.is_empty() {
        eprintln!("skipping: host ancestor instruction files are in scope: {host_files:?}");
        return;
    }
    for args in [&["overhead"][..], &["overhead", "trim"][..]] {
        burn(&tmp)
            .args(args)
            .arg("--project")
            .arg(&project)
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "looked for active CLAUDE.md, CLAUDE.local.md, and AGENTS.md instruction chains",
            ));
    }
}
