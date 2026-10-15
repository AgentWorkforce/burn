//! Seals every burn test process away from the user's real session stores.
//!
//! Linking this crate into a test binary runs [`seal`] before `main`: the
//! process environment stops naming anything real. `HOME` (and
//! `USERPROFILE`) point at a tripwire — a regular *file* — and every
//! variable that relocates a store is removed, so each default path burn,
//! relayhistory or a spawned `burn` binary derives lands under that file:
//!
//! ```text
//! <tmp>/relayburn-test-seal/no-home-in-tests/.local/share/ai-hist/ai-history.db
//! <tmp>/relayburn-test-seal/no-home-in-tests/.agentworkforce/burn/burn.sqlite
//! <tmp>/relayburn-test-seal/no-home-in-tests/.claude/projects/...
//! ```
//!
//! Nothing can be created or opened beneath a file, so a test that forgets
//! an explicit path — or a mutant that drops one — fails with
//! `Not a directory` naming the tripwire instead of reading or writing the
//! user's ai-hist database, burn ledger or harness data. Reads find
//! nothing for the same reason. Tests that need a home or store pass one
//! explicitly (a tempdir), exactly as production embedders do.
//!
//! Every test target links it (`use relayburn_test_seal as _;`); the
//! `every_test_target_links_the_seal` test enforces that.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Variables removed outright: each relocates a store, ledger or provider
/// root away from `HOME`. Behavior flags (`RELAYBURN_CONTENT_STORE`,
/// `RELAYBURN_SDK_NAPI_BUILT`, ...) stay.
const RELOCATING_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
    "HERMES_HOME",
    "OPENCODE_DB",
    "OPENCODE_STORAGE_DIR",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "RELAYBURN_CONTENT_PATH",
    "RELAYBURN_HOME",
    "RELAYBURN_SQLITE_PATH",
];

/// Prefixes removed outright: relayhistory's own settings (`AI_HIST_DB`,
/// ...) and the XDG base directories relayhistory and burn consult before
/// `HOME`.
const RELOCATING_PREFIXES: &[&str] = &["AI_HIST_", "XDG_"];

static TRIPWIRE: OnceLock<PathBuf> = OnceLock::new();
static ORIGINAL_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();

#[ctor::ctor]
fn seal_at_startup() {
    seal();
}

/// Seal the process environment. Idempotent; runs before `main` in every
/// binary that links this crate.
pub fn seal() {
    ORIGINAL_HOME.get_or_init(|| std::env::var_os("HOME").map(PathBuf::from));
    let tripwire = TRIPWIRE.get_or_init(create_tripwire);
    let doomed: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| relocates_a_store(&key.to_string_lossy()))
        .collect();
    for key in doomed {
        std::env::remove_var(key);
    }
    std::env::set_var("HOME", tripwire);
    std::env::set_var("USERPROFILE", tripwire);
}

/// The file every default home resolves to in a sealed process.
pub fn tripwire() -> &'static Path {
    TRIPWIRE.get_or_init(create_tripwire)
}

/// `HOME` as the process received it, before sealing — for proofs that a
/// sealed test left the user's real stores untouched. Never a place to
/// write.
pub fn original_home() -> Option<&'static Path> {
    ORIGINAL_HOME.get().and_then(|home| home.as_deref())
}

/// Compared ASCII-case-insensitively: Windows resolves environment names
/// that way, so `Relayburn_Home` relocates the ledger there just as
/// `RELAYBURN_HOME` does.
fn relocates_a_store(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    RELOCATING_VARS.contains(&key.as_str())
        || RELOCATING_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

fn create_tripwire() -> PathBuf {
    let dir = std::env::temp_dir().join("relayburn-test-seal");
    std::fs::create_dir_all(&dir).expect("create the test-seal directory");
    let tripwire = dir.join("no-home-in-tests");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&tripwire)
        .expect("create the test-seal tripwire file");
    assert!(
        tripwire.is_file(),
        "{} must be a regular file so nothing can be created beneath it",
        tripwire.display()
    );
    tripwire
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_the_tripwire_file() {
        assert_eq!(
            std::env::var_os("HOME").as_deref(),
            Some(tripwire().as_os_str())
        );
        assert_eq!(
            std::env::var_os("USERPROFILE").as_deref(),
            Some(tripwire().as_os_str())
        );
        assert!(tripwire().is_file());
    }

    #[test]
    fn no_store_relocating_variable_survives() {
        let survivors: Vec<_> = std::env::vars()
            .map(|(key, _)| key)
            .filter(|key| relocates_a_store(key))
            .collect();
        assert!(survivors.is_empty(), "unsealed: {survivors:?}");
    }

    #[test]
    fn nothing_can_be_created_under_the_sealed_home() {
        let db = tripwire().join(".local/share/ai-hist");
        let err = std::fs::create_dir_all(&db).expect_err("tripwire must refuse children");
        assert_ne!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert!(!db.exists());
    }

    #[test]
    fn relocating_names_are_recognized() {
        for key in [
            "AI_HIST_DB",
            "RELAYBURN_HOME",
            "RELAYBURN_SQLITE_PATH",
            "XDG_DATA_HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "OPENCODE_DB",
            "Relayburn_Home",
            "ai_hist_db",
            "Codex_Home",
            "xdg_data_home",
        ] {
            assert!(relocates_a_store(key), "{key}");
        }
        for key in [
            "PATH",
            "TMPDIR",
            "NO_COLOR",
            "BURN_GOLDEN",
            "CLAUDECODE",
            "RELAYBURN_CONTENT_STORE",
        ] {
            assert!(!relocates_a_store(key), "{key}");
        }
    }

    /// Every crate's unit tests and every integration test binary link the
    /// seal, so no burn test process runs with a real `HOME`.
    #[test]
    fn every_test_target_links_the_seal() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut unsealed = Vec::new();
        for entry in std::fs::read_dir(crates).unwrap() {
            let krate = entry.unwrap().path();
            if krate.file_name() == Some("relayburn-test-seal".as_ref()) {
                continue;
            }
            for root in ["src/lib.rs", "src/main.rs"] {
                let file = krate.join(root);
                if file.is_file()
                    && !links_seal(&file, "#[cfg(test)]\nuse relayburn_test_seal as _;")
                {
                    unsealed.push(file);
                }
            }
            let Ok(tests) = std::fs::read_dir(krate.join("tests")) else {
                continue;
            };
            for test in tests {
                let file = test.unwrap().path();
                if file.extension() == Some("rs".as_ref())
                    && !links_seal(&file, "\nuse relayburn_test_seal as _;")
                {
                    unsealed.push(file);
                }
            }
        }
        assert!(
            unsealed.is_empty(),
            "these test targets do not link relayburn_test_seal: {unsealed:#?}"
        );
    }

    fn links_seal(file: &Path, line: &str) -> bool {
        std::fs::read_to_string(file).unwrap().contains(line)
    }
}
