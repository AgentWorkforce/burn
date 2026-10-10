//! `burn overhead deltas` — per-inference context-window deltas.
//!
//! Thin presenter over `relayburn_sdk::context_delta`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use relayburn_sdk::{
    context_delta as sdk_context_delta, ContextDelta, ContextDeltaOpts,
    ContextDeltaOwnerRail as OwnerRail, InterveningStep,
};

use crate::cli::{GlobalArgs, OverheadDeltasArgs};
use crate::render::error::report_error;
use crate::render::format::{
    coerce_whole_f64_to_int, format_tokens, format_uint, format_usd, render_table,
};
use crate::render::json::{render_json, stdout_error};
use crate::render::progress::TaskProgress;

pub(super) fn run(
    globals: &GlobalArgs,
    project: Option<PathBuf>,
    since: Option<String>,
    args: OverheadDeltasArgs,
) -> i32 {
    let opts = ContextDeltaOpts {
        session: args.session.clone(),
        project: project
            .as_deref()
            .map(|path| resolve_deltas_project(path).to_string_lossy().into_owned()),
        since,
        top: args.top,
        min_delta: args.min_delta,
        owner: args.owner.into(),
    };
    let progress = TaskProgress::new(globals, "overhead deltas");
    progress.set_task("computing context deltas");
    let deltas = match sdk_context_delta(opts, globals.ledger_path.clone()) {
        Ok(d) => d,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    if globals.json {
        let mut value = match serde_json::to_value(&deltas) {
            Ok(v) => v,
            Err(err) => return report_error(&io::Error::other(err), globals),
        };
        coerce_whole_f64_to_int(&mut value);
        if let Err(err) = render_json(&value) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_deltas(&deltas, args.explain) {
        return report_error(&err, globals);
    }
    0
}

fn render_human_deltas(deltas: &[ContextDelta], explain: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if deltas.is_empty() {
        return handle
            .write_all(b"# no context deltas above threshold\n")
            .map_err(stdout_error);
    }

    let mut table: Vec<Vec<String>> = Vec::with_capacity(deltas.len() + 1);
    table.push(vec![
        "Inference".to_string(),
        "Owner".to_string(),
        "Delta".to_string(),
        "Cost".to_string(),
        "Driver".to_string(),
    ]);
    for d in deltas {
        let inf_label = format!("{}/inf{}", short_turn_label(&d.turn_id), d.inference_idx);
        let owner_label = match &d.owner_rail {
            OwnerRail::Main => "main".to_string(),
            OwnerRail::Subagent { agent_id } => format!("sub:{}", short_agent_label(agent_id)),
        };
        let delta_label = format_signed_tokens(d.delta_tokens);
        let cost_label = format_usd(d.attributed_cost_usd);
        let driver_label = driver_summary(&d.intervening);
        table.push(vec![
            inf_label,
            owner_label,
            delta_label,
            cost_label,
            driver_label,
        ]);
    }
    handle
        .write_all(render_table(&table).as_bytes())
        .map_err(stdout_error)?;
    handle.write_all(b"\n").map_err(stdout_error)?;

    if explain {
        handle.write_all(b"\n").map_err(stdout_error)?;
        for d in deltas {
            let inf_label = format!("{}/inf{}", short_turn_label(&d.turn_id), d.inference_idx);
            let header = format!(
                "{inf_label} — {} steps, prior {} -> current {} tok\n",
                d.intervening.len(),
                format_tokens(d.prior_context_tokens),
                format_tokens(d.current_context_tokens),
            );
            handle.write_all(header.as_bytes()).map_err(stdout_error)?;
            for step in &d.intervening {
                let line = format!("    - {}\n", explain_step(step));
                handle.write_all(line.as_bytes()).map_err(stdout_error)?;
            }
        }
    }

    handle
        .write_all(
            b"\n# token / cost figures are approximate (bytes/4 for tool results,\n\
              # cache-read rate for cost). Compaction rows surface separately and\n\
              # never appear as negative deltas.\n",
        )
        .map_err(stdout_error)?;
    handle.flush().map_err(stdout_error)?;
    Ok(())
}

fn short_turn_label(turn_id: &str) -> String {
    // Turn ids on Claude are `msg-...` UUIDs; trim to a short prefix
    // for the table. Keep the original for JSON output. Use
    // `chars().take(8)` rather than byte slicing so non-ASCII ids
    // (defensive — Claude ids are ASCII, but the helper is generic)
    // don't panic on a mid-byte cut.
    let trimmed = turn_id.trim_start_matches("msg_");
    let trimmed = trimmed.trim_start_matches("msg-");
    let short: String = trimmed.chars().take(8).collect();
    format!("T{short}")
}

fn short_agent_label(agent_id: &str) -> String {
    let trimmed = agent_id.trim_start_matches("agent-");
    trimmed.chars().take(8).collect()
}

fn format_signed_tokens(n: i64) -> String {
    let sign = if n > 0 {
        "+"
    } else if n < 0 {
        "-"
    } else {
        ""
    };
    format!("{sign}{}", format_tokens(n.unsigned_abs()))
}

fn driver_summary(steps: &[InterveningStep]) -> String {
    if steps.is_empty() {
        return "(no intervening leaves)".to_string();
    }
    // Largest step by approx_tokens, with a "N steps" suffix when more
    // than one. Compaction rows always win their summary because
    // freeing tokens is the most explanatory signal.
    if let Some(comp) = steps
        .iter()
        .find(|s| matches!(s, InterveningStep::Compaction { .. }))
    {
        return comp.driver_label();
    }
    let largest = steps
        .iter()
        .max_by_key(|s| s.approx_tokens())
        .expect("non-empty");
    let extra = steps.len().saturating_sub(1);
    if extra == 0 {
        largest.driver_label()
    } else {
        format!(
            "{} (+{extra} more step{})",
            largest.driver_label(),
            if extra == 1 { "" } else { "s" }
        )
    }
}

fn explain_step(step: &InterveningStep) -> String {
    match step {
        InterveningStep::ToolResult {
            tool_use_id,
            tool_name,
            approx_tokens,
            approx_bytes,
            truncated,
        } => format!(
            "tool_result {tool_name} (id={tool_use_id}): ~{} tok / {} bytes{}",
            format_tokens(*approx_tokens),
            format_uint(*approx_bytes),
            if *truncated { " [truncated]" } else { "" },
        ),
        InterveningStep::UserPrompt {
            approx_tokens,
            has_system_reminder,
        } => format!(
            "user prompt: ~{} tok{}",
            format_tokens(*approx_tokens),
            if *has_system_reminder {
                " (with system-reminder)"
            } else {
                ""
            },
        ),
        InterveningStep::SystemReminder {
            source,
            approx_tokens,
        } => format!(
            "system-reminder ({source:?}): ~{} tok",
            format_tokens(*approx_tokens),
        ),
        InterveningStep::Compaction { tokens_freed } => {
            format!("compaction: -{} tok freed", format_tokens(*tokens_freed))
        }
        InterveningStep::Other => "other".to_string(),
    }
}

fn resolve_deltas_project(project: &Path) -> PathBuf {
    if project.is_absolute() {
        project.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| {
                let mut resolved = PathBuf::new();
                for component in cwd.join(project).components() {
                    match component {
                        std::path::Component::CurDir => {}
                        std::path::Component::ParentDir => {
                            resolved.pop();
                        }
                        other => resolved.push(other.as_os_str()),
                    }
                }
                resolved
            })
            .unwrap_or_else(|_| project.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_turn_label_trims_msg_prefix() {
        assert_eq!(short_turn_label("msg_abcdef1234"), "Tabcdef12");
        assert_eq!(short_turn_label("msg-deadbeef"), "Tdeadbeef");
        assert_eq!(short_turn_label("xyz"), "Txyz");
    }

    #[test]
    fn driver_summary_singles_out_compaction() {
        let steps = vec![
            InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 100,
                approx_bytes: 400,
                truncated: false,
            },
            InterveningStep::Compaction { tokens_freed: 5000 },
        ];
        let s = driver_summary(&steps);
        assert!(s.contains("compaction"));
    }

    #[test]
    fn driver_summary_picks_largest_step() {
        let steps = vec![
            InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 100,
                approx_bytes: 400,
                truncated: false,
            },
            InterveningStep::ToolResult {
                tool_use_id: "tu-2".into(),
                tool_name: "Read".into(),
                approx_tokens: 5000,
                approx_bytes: 20_000,
                truncated: false,
            },
        ];
        let s = driver_summary(&steps);
        assert!(s.contains("Read"), "got {s}");
        assert!(s.contains("more"), "got {s}");
    }

    #[test]
    fn format_signed_tokens_handles_positive_and_zero() {
        assert_eq!(format_signed_tokens(0), "0");
        assert!(format_signed_tokens(5_000).starts_with('+'));
    }

    #[test]
    fn resolve_deltas_project_absolutizes_without_resolving_symlinks() {
        let dir = tempfile::Builder::new()
            .prefix("relayburn-project-")
            .tempdir_in(".")
            .expect("temp project");
        let input = Path::new(dir.path().file_name().expect("temp project name"));
        assert!(!input.is_absolute());
        assert_eq!(
            resolve_deltas_project(input),
            std::env::current_dir().expect("cwd").join(input)
        );
        assert_eq!(
            resolve_deltas_project(Path::new(".")),
            std::env::current_dir().expect("cwd")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_deltas_project_preserves_absolute_symlink_spelling() {
        use std::os::unix::fs::symlink;

        let target = tempfile::tempdir().expect("project target");
        let links = tempfile::tempdir().expect("symlink parent");
        let link = links.path().join("project-link");
        symlink(target.path(), &link).expect("project symlink");
        assert_ne!(
            link,
            std::fs::canonicalize(&link).expect("canonical project")
        );
        assert_eq!(resolve_deltas_project(&link), link);
    }
}
