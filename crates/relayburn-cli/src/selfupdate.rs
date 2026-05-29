//! Self-update for the `burn` binary.
//!
//! Two entry points:
//!
//! - [`maybe_offer_update`] — the on-launch check. Called from
//!   `main::dispatch` for every interactive command except `update`
//!   itself and `mcp-server`. Throttled to one network probe per
//!   [`CHECK_INTERVAL_SECS`]; when a newer release exists it prompts the
//!   user to install + restart, and remembers a declined version so it
//!   doesn't nag again until the next release.
//! - the helpers under `burn update` ([`fetch_latest`], [`perform_install`],
//!   [`UpdateState`], [`detect_channel`]) — the manual upgrade path.
//!
//! `burn` ships through two channels (see `CLAUDE.md`): prebuilt npm
//! platform packages (`@relayburn/cli-<platform>`, driven by the
//! `relayburn` umbrella) and `cargo install relayburn-cli` from
//! crates.io. The installer shells out to whichever package manager owns
//! the running binary so npm's / cargo's bookkeeping stays consistent —
//! we never blind-overwrite a binary another tool manages. When the
//! channel can't be determined (a hand-copied binary, a dev build) the
//! update path declines to act and points the user at the manual
//! commands instead.
//!
//! All update state lives in `$RELAYBURN_HOME/update.json`, separate from
//! the SDK's `config.json`, so the SDK's on-disk config schema doesn't
//! grow a CLI-only concern.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::cli::GlobalArgs;
use crate::render::{prompt, ux};

/// Minimum gap between network probes for the on-launch check. The
/// manual `burn update` path ignores this and always hits the network.
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// User-Agent sent with registry requests. crates.io rejects requests
/// without one, and it gives both registries a way to attribute traffic.
const USER_AGENT: &str = concat!(
    "relayburn-cli/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AgentWorkforce/burn)"
);

/// Set on a re-exec'd process so the freshly-installed binary doesn't run
/// the on-launch check again and risk an update loop.
const SKIP_ENV: &str = "RELAYBURN_SKIP_UPDATE_CHECK";

/// Explicit channel hint. The npm wrapper (`packages/relayburn/bin/burn.js`)
/// sets this to `npm` before exec'ing the binary; everything else falls
/// back to path-based detection.
const CHANNEL_ENV: &str = "RELAYBURN_INSTALL_CHANNEL";

/// How the running `burn` was installed — determines which package
/// manager `perform_install` drives and which registry `fetch_latest`
/// queries for the latest version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Installed via `npm i -g relayburn` (prebuilt platform package).
    Npm,
    /// Installed via `cargo install relayburn-cli`.
    Cargo,
    /// Hand-copied binary / dev build — no owning package manager.
    Unknown,
}

impl Channel {
    pub fn label(self) -> &'static str {
        match self {
            Channel::Npm => "npm",
            Channel::Cargo => "cargo",
            Channel::Unknown => "unknown",
        }
    }

    /// The command a user would run to upgrade by hand on this channel.
    fn manual_hint(self) -> &'static str {
        match self {
            Channel::Npm => "npm install -g relayburn@latest",
            Channel::Cargo => "cargo install relayburn-cli --force",
            Channel::Unknown => {
                "reinstall from your package manager (npm i -g relayburn@latest, or cargo install relayburn-cli --force)"
            }
        }
    }
}

/// Persisted update state at `$RELAYBURN_HOME/update.json`.
///
/// Every field is optional / defaulted so a missing or partially-written
/// file degrades to "auto-update on, never checked" rather than erroring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateState {
    /// Whether the on-launch check runs. Toggled by
    /// `burn update toggle-auto-update`. Defaults to `true`.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Unix seconds of the last network probe (throttling anchor).
    #[serde(default)]
    pub last_check: i64,
    /// Latest version seen at `last_check`, cached so checks inside the
    /// throttle window don't re-hit the network.
    #[serde(default)]
    pub latest_known: Option<String>,
    /// Version the user said "no" to on launch. Suppresses re-prompting
    /// until a newer release supersedes it.
    #[serde(default)]
    pub declined_version: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            auto_update: true,
            last_check: 0,
            latest_known: None,
            declined_version: None,
        }
    }
}

impl UpdateState {
    /// Load state from `<home>/update.json`. A missing or unparseable
    /// file yields [`UpdateState::default`] — the update path is
    /// best-effort and never blocks a command over its own state file.
    pub fn load(home: &Path) -> Self {
        let path = state_path(home);
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist state to `<home>/update.json`, creating the home dir if
    /// needed.
    pub fn save(&self, home: &Path) -> io::Result<()> {
        fs::create_dir_all(home)?;
        let path = state_path(home);
        let body = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, body)
    }
}

fn state_path(home: &Path) -> PathBuf {
    home.join("update.json")
}

/// Resolve the relayburn home dir, honoring the global `--ledger-path`
/// override and otherwise deferring to the SDK's env / default resolution.
pub fn home_dir(globals: &GlobalArgs) -> PathBuf {
    globals
        .ledger_path
        .clone()
        .unwrap_or_else(relayburn_sdk::ledger_home)
}

/// The compiled-in version of the running binary.
pub fn current_version() -> Version {
    // CARGO_PKG_VERSION is always valid semver — a parse failure here
    // would be a build-time bug, so an explicit panic message is clearer
    // than a soft fallback that would silently disable updates.
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

/// Determine the install channel. Prefers the explicit [`CHANNEL_ENV`]
/// hint (set by the npm wrapper), then falls back to inspecting the
/// running executable's path.
pub fn detect_channel() -> Channel {
    if let Ok(hint) = std::env::var(CHANNEL_ENV) {
        match hint.as_str() {
            "npm" => return Channel::Npm,
            "cargo" => return Channel::Cargo,
            _ => {}
        }
    }
    match std::env::current_exe() {
        Ok(exe) => {
            // A custom `$CARGO_HOME` relocates cargo-installed binaries out
            // of `~/.cargo/bin`, which the path heuristic below would miss.
            if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
                if !cargo_home.is_empty() && exe.starts_with(&cargo_home) {
                    return Channel::Cargo;
                }
            }
            channel_from_path(&exe.to_string_lossy())
        }
        Err(_) => Channel::Unknown,
    }
}

/// Path-based channel heuristic, split out for testability.
fn channel_from_path(path: &str) -> Channel {
    // npm global installs land the platform binary under a
    // `@relayburn/cli-<platform>` package inside a `node_modules` tree.
    if path.contains("node_modules") && path.contains("@relayburn") {
        return Channel::Npm;
    }
    // `cargo install` drops binaries in `$CARGO_HOME/bin` (default
    // `~/.cargo/bin`).
    if path.contains(".cargo") {
        return Channel::Cargo;
    }
    Channel::Unknown
}

/// Fetch the latest published version from the registry that backs
/// `channel`. Short timeouts keep the on-launch path from ever hanging
/// the user's actual command.
pub fn fetch_latest(channel: Channel, read_timeout: Duration) -> anyhow::Result<Version> {
    let url = match channel {
        Channel::Npm => "https://registry.npmjs.org/relayburn/latest",
        Channel::Cargo => "https://crates.io/api/v1/crates/relayburn-cli",
        Channel::Unknown => anyhow::bail!("cannot determine how `burn` was installed"),
    };

    let agent = ureq::builder()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(read_timeout)
        .user_agent(USER_AGENT)
        .build();

    let body: serde_json::Value = agent
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("update check request failed: {e}"))?
        .into_json()
        .map_err(|e| anyhow::anyhow!("update check response was not valid JSON: {e}"))?;

    extract_version(channel, &body)
}

/// Pull the version string out of a registry response and parse it.
/// Separated from the network call so it can be unit-tested with canned
/// JSON.
fn extract_version(channel: Channel, body: &serde_json::Value) -> anyhow::Result<Version> {
    let raw = match channel {
        // npm `/<pkg>/latest` returns the latest manifest with a
        // top-level `version`.
        Channel::Npm => body.get("version").and_then(|v| v.as_str()),
        // crates.io returns `{ "crate": { "max_stable_version": ... } }`.
        Channel::Cargo => body
            .get("crate")
            .and_then(|c| c.get("max_stable_version"))
            .and_then(|v| v.as_str()),
        Channel::Unknown => None,
    };
    let raw =
        raw.ok_or_else(|| anyhow::anyhow!("registry response was missing a version field"))?;
    Version::parse(raw.trim())
        .map_err(|e| anyhow::anyhow!("registry returned an unparseable version `{raw}`: {e}"))
}

/// Run the package-manager command that upgrades `burn` in place. Inherits
/// stdio so the user sees npm/cargo progress. Returns an error (rather
/// than panicking) so callers can fall back to manual instructions.
pub fn perform_install(globals: &GlobalArgs, channel: Channel) -> anyhow::Result<()> {
    let (program, args): (&str, &[&str]) = match channel {
        Channel::Npm => ("npm", &["install", "-g", "relayburn@latest"]),
        Channel::Cargo => ("cargo", &["install", "relayburn-cli", "--force"]),
        Channel::Unknown => {
            anyhow::bail!(
                "can't tell how `burn` was installed; upgrade manually: {}",
                Channel::Unknown.manual_hint()
            )
        }
    };

    ux::print_info(&format!("Running `{program} {}`…", args.join(" ")), globals);

    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch `{program}` (is it on your PATH?): {e}"))?;

    if !status.success() {
        anyhow::bail!("`{program}` exited with {status}");
    }
    Ok(())
}

/// The on-launch update check. Best-effort and silent on every failure
/// path — it must never get in the way of the command the user actually
/// typed.
pub fn maybe_offer_update(globals: &GlobalArgs) {
    // Re-exec guard: a freshly-installed binary skips the check.
    if std::env::var_os(SKIP_ENV).is_some() {
        return;
    }
    // No prompts in machine-readable mode or when any of our standard
    // streams isn't a terminal: a piped stdout (`burn summary | cat`) means
    // the user isn't watching an interactive session, so we must not block
    // on a prompt.
    if globals.json
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
        || ux::term_is_dumb()
    {
        return;
    }

    let channel = detect_channel();
    // Nothing we can offer to install — don't probe or prompt.
    if channel == Channel::Unknown {
        return;
    }

    let home = home_dir(globals);
    let mut state = UpdateState::load(&home);
    if !state.auto_update {
        return;
    }

    let current = current_version();
    let latest = match resolve_latest(channel, &home, &mut state) {
        Some(v) => v,
        None => return,
    };

    if latest <= current {
        return;
    }
    let latest_label = latest.to_string();
    // Already said no to exactly this version — wait for the next release.
    if state.declined_version.as_deref() == Some(latest_label.as_str()) {
        return;
    }

    let question =
        format!("A new burn is available: {current} → {latest}. Update and restart now?");
    match prompt::confirm(globals, &question, true) {
        Ok(true) => {}
        Ok(false) => {
            state.declined_version = Some(latest_label.clone());
            let _ = state.save(&home);
            ux::print_info(
                "Staying on the current version. I'll ask again on the next release — or run `burn update` anytime.",
                globals,
            );
            return;
        }
        // Interrupted (Ctrl-C) or prompt I/O error: don't record a
        // decline, just continue — we'll ask again next launch.
        Err(_) => return,
    }

    match perform_install(globals, channel) {
        Ok(()) => {
            state.latest_known = Some(latest_label);
            state.declined_version = None;
            let _ = state.save(&home);
            ux::print_success(&format!("Updated to burn {latest}. Restarting…"), globals);
            // On success this replaces the process and never returns; if
            // it does return, the re-exec failed and we fall through.
            let err = reexec();
            ux::print_warning(
                &format!(
                    "Couldn't restart automatically ({err}). Re-run your command to use {latest}."
                ),
                globals,
            );
        }
        Err(e) => {
            ux::print_error(&format!("Update failed: {e}"), globals);
        }
    }
}

/// Resolve the latest version for the on-launch path, honoring the
/// throttle window. Inside the window we trust the cached `latest_known`;
/// outside it we probe the network and update `last_check` (even on
/// failure, so a flaky network doesn't probe on every launch). Persists
/// any state change. Returns `None` when nothing usable is known.
fn resolve_latest(channel: Channel, home: &Path, state: &mut UpdateState) -> Option<Version> {
    let now = now_unix();
    // Clock skew or a hand-edited `last_check` in the future would
    // otherwise suppress probes until wall-clock time caught up; treat a
    // future stamp as "due now".
    let within_window = state.last_check <= now && now - state.last_check < CHECK_INTERVAL_SECS;
    if within_window {
        return state
            .latest_known
            .as_deref()
            .and_then(|s| Version::parse(s).ok());
    }

    match fetch_latest(channel, Duration::from_secs(4)) {
        Ok(v) => {
            state.last_check = now;
            state.latest_known = Some(v.to_string());
            let _ = state.save(home);
            Some(v)
        }
        Err(_) => {
            state.last_check = now;
            let _ = state.save(home);
            None
        }
    }
}

/// Re-launch the (now updated) binary with the original arguments,
/// replacing the current process on Unix. Only returns on failure.
///
/// `exe` must be resolved *before* the installer runs — see
/// [`maybe_offer_update`] for why.
fn reexec(exe: PathBuf) -> io::Error {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns if it fails to replace the image.
        Command::new(exe).args(args).env(SKIP_ENV, "1").exec()
    }

    #[cfg(not(unix))]
    {
        match Command::new(exe).args(args).env(SKIP_ENV, "1").status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(0)),
            Err(e) => e,
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn channel_from_path_recognizes_npm() {
        assert_eq!(
            channel_from_path("/usr/local/lib/node_modules/@relayburn/cli-darwin-arm64/bin/burn"),
            Channel::Npm
        );
    }

    #[test]
    fn channel_from_path_recognizes_cargo() {
        assert_eq!(
            channel_from_path("/Users/dev/.cargo/bin/burn"),
            Channel::Cargo
        );
    }

    #[test]
    fn channel_from_path_unknown_for_handcopied() {
        assert_eq!(channel_from_path("/usr/local/bin/burn"), Channel::Unknown);
    }

    #[test]
    fn extract_version_reads_npm_shape() {
        let body = serde_json::json!({ "name": "relayburn", "version": "3.1.2" });
        assert_eq!(
            extract_version(Channel::Npm, &body).unwrap(),
            Version::parse("3.1.2").unwrap()
        );
    }

    #[test]
    fn extract_version_reads_crates_shape() {
        let body = serde_json::json!({
            "crate": { "max_stable_version": "3.1.2", "newest_version": "3.2.0-next.1" }
        });
        assert_eq!(
            extract_version(Channel::Cargo, &body).unwrap(),
            Version::parse("3.1.2").unwrap()
        );
    }

    #[test]
    fn extract_version_errors_on_missing_field() {
        let body = serde_json::json!({ "nope": true });
        assert!(extract_version(Channel::Npm, &body).is_err());
        assert!(extract_version(Channel::Cargo, &body).is_err());
    }

    #[test]
    fn state_roundtrips_and_defaults() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        // Missing file → defaults (auto-update on).
        let loaded = UpdateState::load(home);
        assert!(loaded.auto_update);
        assert_eq!(loaded.last_check, 0);
        assert!(loaded.latest_known.is_none());

        let state = UpdateState {
            auto_update: false,
            last_check: 1234,
            latest_known: Some("3.1.0".into()),
            declined_version: Some("3.1.0".into()),
        };
        state.save(home).unwrap();

        let reloaded = UpdateState::load(home);
        assert!(!reloaded.auto_update);
        assert_eq!(reloaded.last_check, 1234);
        assert_eq!(reloaded.latest_known.as_deref(), Some("3.1.0"));
        assert_eq!(reloaded.declined_version.as_deref(), Some("3.1.0"));
    }

    #[test]
    fn malformed_state_falls_back_to_default() {
        let tmp = TempDir::new().unwrap();
        fs::write(state_path(tmp.path()), "{ not json").unwrap();
        let loaded = UpdateState::load(tmp.path());
        assert!(loaded.auto_update);
    }

    #[test]
    fn partial_state_keeps_auto_update_default_on() {
        // A file that only sets last_check should still default
        // auto_update to true rather than false.
        let tmp = TempDir::new().unwrap();
        fs::write(state_path(tmp.path()), r#"{"last_check":42}"#).unwrap();
        let loaded = UpdateState::load(tmp.path());
        assert!(loaded.auto_update);
        assert_eq!(loaded.last_check, 42);
    }
}
