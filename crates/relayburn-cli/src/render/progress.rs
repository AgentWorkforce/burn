//! TTY-only progress helpers.
//!
//! Human runs get a stderr spinner while long-running work is in flight.
//! JSON mode and redirected stderr stay quiet and scriptable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use relayburn_sdk::IngestOptions;

use crate::cli::GlobalArgs;
use crate::render::ux;

#[derive(Clone)]
pub struct TaskProgress {
    inner: Arc<Inner>,
}

struct Inner {
    bar: Option<ProgressBar>,
}

impl TaskProgress {
    pub fn new(globals: &GlobalArgs, label: impl Into<String>) -> Self {
        let color = ux::colors_enabled(globals);
        let pretty = ux::stderr_is_pretty(globals);
        let bar = pretty.then(|| spinner(label.into(), color));
        Self {
            inner: Arc::new(Inner { bar }),
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

    pub fn ingest_options(&self, ledger_home: Option<PathBuf>) -> IngestOptions {
        let on_progress = self.is_visible().then(|| {
            let progress = self.clone();
            Box::new(move |message: &str| {
                progress.set_task(message.to_string());
            }) as Box<dyn Fn(&str) + Send + Sync>
        });
        IngestOptions {
            on_progress,
            ledger_home,
            ..IngestOptions::default()
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
