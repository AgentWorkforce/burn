//! Small CLI design-system helpers.
//!
//! Keep these helpers conservative: they style human TTY output, but fall back
//! to the historic script-friendly strings for JSON, pipes, and dumb terminals.

#![allow(dead_code)]

use std::io::{self, IsTerminal, Write};

use console::style;

use crate::cli::GlobalArgs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Success,
    Error,
    Warning,
    Info,
}

impl MessageKind {
    fn icon(self) -> &'static str {
        match self {
            Self::Success => "✓",
            Self::Error => "✕",
            Self::Warning => "⚠",
            Self::Info => "ℹ",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

pub fn colors_enabled(globals: &GlobalArgs) -> bool {
    !globals.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("CLICOLOR").map(|v| v != "0").unwrap_or(true)
        && !term_is_dumb()
}

pub fn stderr_is_pretty(globals: &GlobalArgs) -> bool {
    !globals.json && io::stderr().is_terminal() && !term_is_dumb()
}

pub fn term_is_dumb() -> bool {
    std::env::var("TERM")
        .map(|term| term.eq_ignore_ascii_case("dumb"))
        .unwrap_or(false)
}

pub fn format_message(kind: MessageKind, message: &str, globals: &GlobalArgs) -> String {
    if !stderr_is_pretty(globals) {
        return match kind {
            MessageKind::Warning => format!("burn: warning: {message}"),
            MessageKind::Error => format!("burn: {message}"),
            MessageKind::Success | MessageKind::Info => message.to_string(),
        };
    }

    let color = colors_enabled(globals);
    let icon = paint_kind(kind, kind.icon(), color, true);
    let label = paint_kind(kind, kind.label(), color, true);
    let rail = paint_kind(kind, "│", color, false);
    let mut lines = message.lines();
    let first = lines.next().unwrap_or(message).trim();

    let mut out = format!("{icon} {label}");
    if !first.is_empty() {
        out.push('\n');
        out.push_str(&format!("  {rail} {first}"));
    }
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&format!("  {rail} {line}"));
    }
    out
}

pub fn print_message(kind: MessageKind, message: &str, globals: &GlobalArgs) {
    let _ = writeln!(io::stderr(), "{}", format_message(kind, message, globals));
}

pub fn print_success(message: &str, globals: &GlobalArgs) {
    print_message(MessageKind::Success, message, globals);
}

pub fn print_error(message: &str, globals: &GlobalArgs) {
    print_message(MessageKind::Error, message, globals);
}

pub fn print_warning(message: &str, globals: &GlobalArgs) {
    print_message(MessageKind::Warning, message, globals);
}

pub fn print_info(message: &str, globals: &GlobalArgs) {
    print_message(MessageKind::Info, message, globals);
}

pub fn section(title: &str, globals: &GlobalArgs) -> String {
    if !colors_enabled(globals) {
        return title.to_string();
    }
    style(title).bold().cyan().to_string()
}

pub fn dim(value: impl ToString, globals: &GlobalArgs) -> String {
    let value = value.to_string();
    if colors_enabled(globals) {
        style(value).dim().to_string()
    } else {
        value
    }
}

fn paint_kind(kind: MessageKind, value: &str, color: bool, bold: bool) -> String {
    if !color {
        return value.to_string();
    }
    let styled = style(value.to_string());
    let styled = match kind {
        MessageKind::Success => styled.green(),
        MessageKind::Error => styled.red(),
        MessageKind::Warning => styled.yellow(),
        MessageKind::Info => styled.cyan(),
    };
    if bold {
        styled.bold().to_string()
    } else {
        styled.dim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piped_globals() -> GlobalArgs {
        GlobalArgs {
            json: false,
            ledger_path: None,
            no_color: false,
        }
    }

    #[test]
    fn non_tty_warning_keeps_scriptable_prefix() {
        assert_eq!(
            format_message(MessageKind::Warning, "heads up", &piped_globals()),
            "burn: warning: heads up"
        );
    }

    #[test]
    fn non_tty_error_keeps_scriptable_prefix() {
        assert_eq!(
            format_message(MessageKind::Error, "boom", &piped_globals()),
            "burn: boom"
        );
    }
}
