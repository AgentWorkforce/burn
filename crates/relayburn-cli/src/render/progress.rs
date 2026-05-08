//! TTY-only progress and warning helpers.
//!
//! Human runs get a stderr spinner while long-running work is in flight.
//! JSON mode and redirected stderr keep the old quiet/scriptable behavior.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use console::style;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use relayburn_sdk::RawIngestOptions;

use crate::cli::GlobalArgs;
use crate::render::ux;

#[derive(Clone)]
pub struct TaskProgress {
    inner: Arc<Inner>,
}

struct Inner {
    bar: Option<ProgressBar>,
    color: bool,
    pretty_warnings: bool,
}

impl TaskProgress {
    pub fn new(globals: &GlobalArgs, label: impl Into<String>) -> Self {
        let color = ux::colors_enabled(globals);
        let pretty = ux::stderr_is_pretty(globals);
        let bar = pretty.then(|| spinner(label.into(), color));
        Self {
            inner: Arc::new(Inner {
                bar,
                color,
                pretty_warnings: pretty,
            }),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.inner.bar.is_some()
    }

    pub fn set_task(&self, message: impl Into<String>) {
        if let Some(bar) = &self.inner.bar {
            bar.set_message(message.into());
        }
    }

    pub fn finish_and_clear(&self) {
        if let Some(bar) = &self.inner.bar {
            bar.finish_and_clear();
        }
    }

    pub fn suspend<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        if let Some(bar) = &self.inner.bar {
            bar.suspend(f);
        } else {
            f();
        }
    }

    pub fn warn(&self, body: &str) {
        self.suspend(|| {
            if self.inner.pretty_warnings {
                eprintln!("{}", format_warning(body, self.inner.color));
            } else {
                eprintln!("burn: warning: {body}");
            }
        });
    }

    pub fn ingest_options(&self, ledger_home: Option<PathBuf>) -> RawIngestOptions {
        let on_progress = self.is_visible().then(|| {
            let progress = self.clone();
            Box::new(move |message: &str| {
                progress.set_task(message.to_string());
            }) as Box<dyn Fn(&str) + Send + Sync>
        });
        let on_warn = {
            let progress = self.clone();
            Some(Box::new(move |body: &str| {
                progress.warn(body);
            }) as Box<dyn Fn(&str) + Send + Sync>)
        };

        RawIngestOptions {
            on_progress,
            on_warn,
            ledger_home,
            ..RawIngestOptions::default()
        }
    }

    pub fn quiet_ingest_options(ledger_home: Option<PathBuf>) -> RawIngestOptions {
        RawIngestOptions {
            on_warn: Some(Box::new(ignore_warning) as Box<dyn Fn(&str) + Send + Sync>),
            ledger_home,
            ..RawIngestOptions::default()
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

fn spinner(label: String, color: bool) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
    let template = if color {
        "{spinner:.magenta} {prefix:.cyan} {msg}"
    } else {
        "{spinner} {prefix} {msg}"
    };
    let style = ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
    bar.set_style(style);
    bar.set_prefix(label);
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
}

fn ignore_warning(_: &str) {}

fn format_warning(body: &str, color: bool) -> String {
    let mut lines = body.lines();
    let first = lines.next().unwrap_or(body).trim();
    let (scope, detail) = first.split_once(':').unwrap_or(("burn", first));
    let scope = scope.trim();
    let detail = detail.trim();

    let icon = paint("⚠", color, |s| style(s).yellow().bold().to_string());
    let title = paint(format!("{scope} ingest warning"), color, |s| {
        style(s).yellow().bold().to_string()
    });
    let rail = paint("│", color, |s| style(s).yellow().dim().to_string());

    let mut out = format!("{icon} {title}");
    if !detail.is_empty() {
        out.push('\n');
        out.push_str(&format!("  {rail} {detail}"));
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

fn paint<S, F>(value: S, color: bool, apply: F) -> String
where
    S: ToString,
    F: FnOnce(String) -> String,
{
    let value = value.to_string();
    if color {
        apply(value)
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::format_warning;

    #[test]
    fn warning_formatter_reframes_gap_warning() {
        let body = "claude: 7 sessions logged tool calls without any observed tool_result content (381 tool calls).\n  Likely cause: still running.\n  Counts decay later.";
        let rendered = format_warning(body, false);
        assert_eq!(
            rendered,
            "⚠ claude ingest warning\n  │ 7 sessions logged tool calls without any observed tool_result content (381 tool calls).\n  │ Likely cause: still running.\n  │ Counts decay later."
        );
    }
}
