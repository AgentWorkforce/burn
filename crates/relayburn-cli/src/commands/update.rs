//! `burn update` — manual upgrade + auto-update toggle.
//!
//! Three shapes, all thin presenters over [`crate::selfupdate`]:
//!
//! - `burn update`                     — upgrade to the latest release.
//! - `burn update --check`             — report availability, install nothing.
//! - `burn update toggle-auto-update`  — flip the on-launch check on/off.
//!
//! The heavy lifting (channel detection, registry query, package-manager
//! shell-out, on-disk state) lives in [`crate::selfupdate`]; this file
//! only maps CLI flags onto those calls and renders the result.

use std::time::Duration;

use serde_json::json;

use crate::cli::{GlobalArgs, ToggleAutoUpdateArgs, UpdateAction, UpdateArgs};
use crate::render::error::report_error;
use crate::render::ux;
use crate::selfupdate::{self, Channel, UpdateState};

/// Network budget for the manual path. Longer than the on-launch probe —
/// the user explicitly asked, so it's fine to wait a beat.
const MANUAL_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(globals: &GlobalArgs, args: UpdateArgs) -> i32 {
    let UpdateArgs {
        check,
        force,
        action,
    } = args;

    match action {
        Some(_) if check || force => report_error(
            &anyhow::anyhow!(
                "`burn update --check` and `burn update --force` cannot be combined \
                 with a subcommand"
            ),
            globals,
        ),
        Some(UpdateAction::ToggleAutoUpdate(toggle)) => run_toggle(globals, toggle),
        None => run_update(globals, check, force),
    }
}

fn run_update(globals: &GlobalArgs, check_only: bool, force: bool) -> i32 {
    let channel = selfupdate::detect_channel();
    let current = selfupdate::current_version();

    // Without a known install channel we can neither pick a registry to
    // query nor an installer to run, so bail before the network probe with
    // the manual commands rather than a cryptic request error. Covers both
    // `--check` and the install path.
    if channel == Channel::Unknown {
        return report_error(&unknown_channel_error(), globals);
    }

    let latest = match selfupdate::fetch_latest(channel, MANUAL_TIMEOUT) {
        Ok(v) => v,
        Err(err) => return report_error(&err, globals),
    };

    // Refresh cached state so the on-launch path benefits from this probe.
    let home = selfupdate::home_dir(globals);
    let mut state = UpdateState::load(&home);
    state.last_check = now_unix();
    state.latest_known = Some(latest.to_string());
    let _ = state.save(&home);

    let available = latest > current;

    if check_only {
        if globals.json {
            let _ = print_json(&json!({
                "current": current.to_string(),
                "latest": latest.to_string(),
                "updateAvailable": available,
                "channel": channel.label(),
            }));
        } else if available {
            ux::print_info(
                &format!("Update available: {current} → {latest} (install with `burn update`)."),
                globals,
            );
        } else {
            ux::print_success(&format!("burn is up to date ({current})."), globals);
        }
        return 0;
    }

    if !available && !force {
        ux::print_success(&format!("burn is already up to date ({current})."), globals);
        return 0;
    }

    match selfupdate::perform_install(globals, channel) {
        Ok(()) => {
            // Clear any earlier on-launch decline now that we're current.
            state.declined_version = None;
            let _ = state.save(&home);
            ux::print_success(&format!("Updated to burn {latest}."), globals);
            0
        }
        Err(err) => report_error(&err, globals),
    }
}

fn run_toggle(globals: &GlobalArgs, toggle: ToggleAutoUpdateArgs) -> i32 {
    let home = selfupdate::home_dir(globals);
    let mut state = UpdateState::load(&home);

    // `--on` / `--off` set explicitly; bare `toggle-auto-update` flips.
    state.auto_update = if toggle.on {
        true
    } else if toggle.off {
        false
    } else {
        !state.auto_update
    };

    if let Err(err) = state.save(&home) {
        return report_error(&err, globals);
    }

    if globals.json {
        let _ = print_json(&json!({ "autoUpdate": state.auto_update }));
    } else if state.auto_update {
        ux::print_success(
            "Auto-update is ON. burn will offer to upgrade itself on launch.",
            globals,
        );
    } else {
        ux::print_success(
            "Auto-update is OFF. Run `burn update` to upgrade manually.",
            globals,
        );
    }
    0
}

/// Shared "we don't know how this was installed" guidance for the
/// `Channel::Unknown` path.
fn unknown_channel_error() -> anyhow::Error {
    anyhow::anyhow!(
        "can't tell how `burn` was installed; upgrade manually with \
         `npm install -g relayburn@latest` or `cargo install relayburn-cli --force`"
    )
}

fn print_json(value: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value).map_err(std::io::Error::other)?;
    out.write_all(b"\n")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
