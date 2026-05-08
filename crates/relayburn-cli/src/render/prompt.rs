//! Interactive prompt wrappers.
//!
//! Current commands remain flag-driven for automation. These helpers make future
//! interactive flows consistent and keep dialoguer details out of command code.

#![allow(dead_code)]

use std::io::{self, IsTerminal};

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

use crate::cli::GlobalArgs;
use crate::render::ux;

pub fn confirm(globals: &GlobalArgs, prompt: &str, default: bool) -> io::Result<bool> {
    if !interactive(globals) {
        return Ok(default);
    }
    Confirm::with_theme(&theme(globals))
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(io::Error::other)
}

pub fn input(globals: &GlobalArgs, prompt: &str, default: Option<&str>) -> io::Result<String> {
    if !interactive(globals) {
        return Ok(default.unwrap_or_default().to_string());
    }

    let theme = theme(globals);
    let mut input = Input::<String>::with_theme(&theme).with_prompt(prompt);
    if let Some(default) = default {
        input = input.default(default.to_string());
    }
    input.interact_text().map_err(io::Error::other)
}

pub fn select(
    globals: &GlobalArgs,
    prompt: &str,
    items: &[impl AsRef<str>],
    default: Option<usize>,
) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    if !interactive(globals) {
        return Ok(default);
    }

    let labels: Vec<&str> = items.iter().map(|item| item.as_ref()).collect();
    let theme = theme(globals);
    let mut select = Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(&labels);
    if let Some(default) = default {
        select = select.default(default.min(items.len() - 1));
    }
    select.interact_opt().map_err(io::Error::other)
}

fn interactive(globals: &GlobalArgs) -> bool {
    !globals.json
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && !ux::term_is_dumb()
}

fn theme(_globals: &GlobalArgs) -> ColorfulTheme {
    ColorfulTheme::default()
}
