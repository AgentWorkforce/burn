//! `burn init` — setup helpers for collectors that need opt-in
//! configuration before burn can see their data.
//!
//! `burn init copilot` prints the exact `COPILOT_OTEL_FILE_EXPORTER_PATH`
//! setup for the GitHub Copilot CLI collector (#14). It never touches the
//! user's shell rc files — Copilot's exporter is opt-in and which rc file
//! to edit is the user's call, so this just prints copy-pasteable lines.

use crate::cli::InitArgs;

/// Print the opt-in setup for one collector (`burn init copilot`).
/// Read-only: never edits shell rc files, only prints copy-pasteable lines.
pub fn run(args: InitArgs) -> i32 {
    match args.action {
        crate::cli::InitAction::Copilot => {
            print!(
                "GitHub Copilot CLI collector setup\n\
                 \n\
                 Copilot CLI only emits usage data when its OpenTelemetry file\n\
                 exporter is enabled, and nothing is recoverable retroactively —\n\
                 export must be on before sessions are captured.\n\
                 \n\
                 1. Point the exporter at a file burn scans:\n\
                 \n\
                 \x20   export COPILOT_OTEL_FILE_EXPORTER_PATH=\"$HOME/.copilot/otel/copilot.jsonl\"\n\
                 \n\
                 \x20   Add that line to your shell rc file (~/.zshrc, ~/.bashrc, …) so\n\
                 \x20   new shells pick it up — burn ingest only scans Copilot\n\
                 \x20   files while this variable is set. Any path works.\n\
                 \n\
                 2. Restart your shell (or `source` the rc file), then run Copilot CLI.\n\
                 \n\
                 3. Ingest as usual — `burn ingest` picks the spans up automatically.\n\
                 \n\
                 Coverage note: OTEL spans carry per-API-call token usage (input,\n\
                 output, cache read/write, reasoning) and the model, but no tool-call\n\
                 content, so Copilot turns report as usage-only fidelity.\n"
            );
            0
        }
    }
}
