//! `burn ingest` end to end against a sealed provider home: which ledger
//! config it honours, and how `--watch` reports and stops.

// Seal this test process away from the user's real stores before any test runs.
use relayburn_test_seal as _;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

/// A provider home holding one Claude session, a `RELAYBURN_HOME`, and a
/// separate `--ledger-path` whose config keeps no content.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().expect("sandbox");
        let project = dir.path().join("home/.claude/projects/-tmp-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(
            fixture("tests/fixtures/claude/simple-turn.jsonl"),
            project.join("11111111-1111-1111-1111-111111111111.jsonl"),
        )
        .unwrap();
        let ledger = dir.path().join("ledger");
        std::fs::create_dir_all(&ledger).unwrap();
        std::fs::write(ledger.join("config.json"), r#"{"content":{"store":"off"}}"#).unwrap();
        std::fs::create_dir_all(dir.path().join("relayburn-home")).unwrap();
        Self { dir }
    }

    fn ledger(&self) -> PathBuf {
        self.dir.path().join("ledger")
    }

    fn burn(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_burn"));
        command
            .arg("--ledger-path")
            .arg(self.ledger())
            .args(args)
            .env("HOME", self.dir.path().join("home"))
            .env("RELAYBURN_HOME", self.dir.path().join("relayburn-home"))
            .env("NO_COLOR", "1")
            .env_remove("RELAYBURN_CONTENT_STORE")
            .env_remove("AI_HIST_DB")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("OPENCODE_DB");
        command
    }

    /// `(turns, content rows)` the ledger holds.
    fn rows(&self) -> (u64, u64) {
        let output = self
            .burn(&["--json", "state", "status"])
            .output()
            .expect("state status");
        assert!(output.status.success(), "{output:?}");
        let status: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
        let count = |v: &Value| v.as_u64().expect("row count");
        (
            count(&status["burn"]["rows"]["turns"]),
            count(&status["content"]["rows"]),
        )
    }
}

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

#[test]
fn ingest_honours_the_config_of_the_ledger_path() {
    for quiet in [false, true] {
        let sandbox = Sandbox::new();
        let mut args = vec!["ingest"];
        if quiet {
            args.push("--quiet");
        }
        let output = sandbox.burn(&args).output().expect("ingest");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            sandbox.rows(),
            (1, 0),
            "quiet={quiet}: the turn lands, and the ledger path's config keeps no content"
        );
    }
}

/// A running `burn ingest --watch` and the stderr lines it writes.
struct Watch {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Watch {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn watch");
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Wait for a stderr line containing `needle`.
    fn expect(&mut self, needle: &str) {
        while !self.seen.iter().any(|line| line.contains(needle)) {
            match self.lines.recv_timeout(Duration::from_secs(120)) {
                Ok(line) => self.seen.push(line),
                Err(_) => {
                    let _ = self.child.kill();
                    panic!("no stderr line containing {needle:?}; saw {:?}", self.seen);
                }
            }
        }
    }

    /// Send SIGTERM and wait for the exit.
    fn terminate(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("kill");
        assert!(sent.success());
        for _ in 0..200 {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        panic!("watch did not stop on SIGTERM; saw {:?}", self.seen);
    }
}

#[cfg(unix)]
#[test]
fn watch_reports_each_tick_and_stops_cleanly_on_sigterm() {
    let sandbox = Sandbox::new();
    let mut watch =
        Watch::spawn(sandbox.burn(&["ingest", "--watch", "--no-fsevents", "--interval", "200"]));
    watch.expect("[burn] ingest: foreground ingest watching (polling every 200ms); Ctrl-C to stop");
    watch.expect("[burn] ingest: ingested 1 session (+1 turn)");
    let status = watch.terminate();
    assert!(status.success(), "{status:?}");
    assert_eq!(
        sandbox.rows(),
        (1, 0),
        "the watch loop reads the ledger path's config"
    );
}

#[cfg(unix)]
#[test]
fn watch_follows_filesystem_events_by_default() {
    let sandbox = Sandbox::new();
    let mut watch = Watch::spawn(sandbox.burn(&["ingest", "--watch"]));
    watch.expect(
        "[burn] ingest: foreground ingest watching (FS events with polling fallback); Ctrl-C to stop",
    );
    watch.expect("[burn] ingest: ingested 1 session (+1 turn)");
    assert!(watch.terminate().success());
}

#[test]
fn watch_rejects_a_zero_interval() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .burn(&["ingest", "--watch", "--interval", "0"])
        .output()
        .expect("ingest");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("ingest --interval must be a positive integer in milliseconds"));
}

/// A `burn` run that inherits the test process environment instead of
/// naming its homes resolves every default under the sealed `HOME` and
/// fails there, so a test that forgets its paths can never sync the
/// user's real relayhistory store or write their ledger.
#[test]
fn ingest_without_explicit_homes_fails_at_the_tripwire() {
    let output = Command::new(env!("CARGO_BIN_EXE_burn"))
        .arg("ingest")
        .env("NO_COLOR", "1")
        .output()
        .expect("burn ingest");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "ingest succeeded: {output:?}");
    let tripwire = relayburn_test_seal::tripwire().display().to_string();
    assert!(
        stderr.contains(&tripwire) || stderr.contains("Not a directory"),
        "{stderr}"
    );
}
