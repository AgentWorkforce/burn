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
    write_human_deltas(&mut handle, deltas, explain)
        .and_then(|()| handle.flush())
        .map_err(stdout_error)
}

fn write_human_deltas<W: Write>(
    out: &mut W,
    deltas: &[ContextDelta],
    explain: bool,
) -> io::Result<()> {
    if deltas.is_empty() {
        return out.write_all(b"# no context deltas above threshold\n");
    }
    out.write_all(render_table(&deltas_table(deltas)).as_bytes())?;
    out.write_all(b"\n")?;
    if explain {
        out.write_all(b"\n")?;
        for d in deltas {
            write_explain(out, d)?;
        }
    }
    out.write_all(
        b"\n# token / cost figures are approximate (bytes/4 for tool results,\n\
          # cache-read rate for cost). Compaction rows surface separately and\n\
          # never appear as negative deltas.\n",
    )
}

fn deltas_table(deltas: &[ContextDelta]) -> Vec<Vec<String>> {
    let mut table: Vec<Vec<String>> = Vec::with_capacity(deltas.len() + 1);
    table.push(
        ["Inference", "Owner", "Delta", "Cost", "Driver"]
            .map(String::from)
            .to_vec(),
    );
    for d in deltas {
        table.push(vec![
            inference_label(d),
            owner_label(&d.owner_rail),
            format_signed_tokens(d.delta_tokens),
            format_usd(d.attributed_cost_usd),
            driver_summary(&d.intervening),
        ]);
    }
    table
}

fn write_explain<W: Write>(out: &mut W, d: &ContextDelta) -> io::Result<()> {
    writeln!(
        out,
        "{} — {} steps, prior {} -> current {} tok",
        inference_label(d),
        d.intervening.len(),
        format_tokens(d.prior_context_tokens),
        format_tokens(d.current_context_tokens),
    )?;
    for step in &d.intervening {
        writeln!(out, "    - {}", explain_step(step))?;
    }
    Ok(())
}

fn inference_label(d: &ContextDelta) -> String {
    format!("{}/inf{}", short_turn_label(&d.turn_id), d.inference_idx)
}

fn owner_label(rail: &OwnerRail) -> String {
    match rail {
        OwnerRail::Main => "main".to_string(),
        OwnerRail::Subagent { agent_id } => format!("sub:{}", short_agent_label(agent_id)),
    }
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

    fn sample_delta(owner_rail: OwnerRail, intervening: Vec<InterveningStep>) -> ContextDelta {
        ContextDelta {
            session_id: "sess-1".into(),
            turn_id: "msg_abcdef1234".into(),
            inference_idx: 3,
            owner_rail,
            prior_context_tokens: 1_000,
            current_context_tokens: 6_000,
            delta_tokens: 5_000,
            intervening,
            attributed_cost_usd: 0.0,
        }
    }

    fn render(deltas: &[ContextDelta], explain: bool) -> String {
        let mut buf = Vec::new();
        write_human_deltas(&mut buf, deltas, explain).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn write_human_deltas_reports_empty_result() {
        assert_eq!(render(&[], true), "# no context deltas above threshold\n");
    }

    #[test]
    fn write_human_deltas_table_without_explain() {
        let d = sample_delta(
            OwnerRail::Subagent {
                agent_id: "agent-0123456789".into(),
            },
            Vec::new(),
        );
        let out = render(&[d], false);
        assert!(out.contains("Inference"), "got {out}");
        assert!(out.contains("Tabcdef12/inf3"), "got {out}");
        assert!(out.contains("sub:01234567"), "got {out}");
        assert!(out.contains("(no intervening leaves)"), "got {out}");
        assert!(!out.contains(" steps, prior "), "got {out}");
        assert!(
            out.ends_with("# never appear as negative deltas.\n"),
            "got {out}"
        );
    }

    #[test]
    fn write_human_deltas_explain_lists_every_step_kind() {
        let steps = vec![
            InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 100,
                approx_bytes: 400,
                truncated: true,
            },
            InterveningStep::UserPrompt {
                approx_tokens: 20,
                has_system_reminder: true,
            },
            InterveningStep::UserPrompt {
                approx_tokens: 30,
                has_system_reminder: false,
            },
            InterveningStep::SystemReminder {
                source: relayburn_sdk::ReminderSource::Other,
                approx_tokens: 40,
            },
            InterveningStep::Compaction { tokens_freed: 50 },
            InterveningStep::Other,
        ];
        let out = render(&[sample_delta(OwnerRail::Main, steps)], true);
        let explain: Vec<&str> = out
            .lines()
            .skip_while(|line| !line.contains(" steps, prior "))
            .take(7)
            .collect();
        assert_eq!(
            explain,
            vec![
                "Tabcdef12/inf3 — 6 steps, prior 1.0k -> current 6.0k tok",
                "    - tool_result Bash (id=tu-1): ~100 tok / 400 bytes [truncated]",
                "    - user prompt: ~20 tok (with system-reminder)",
                "    - user prompt: ~30 tok",
                "    - system-reminder (Other): ~40 tok",
                "    - compaction: -50 tok freed",
                "    - other",
            ]
        );
        assert!(out.contains("main"), "got {out}");
    }

    #[test]
    fn explain_step_tool_result_omits_truncated_marker_when_complete() {
        let step = InterveningStep::ToolResult {
            tool_use_id: "tu-2".into(),
            tool_name: "Read".into(),
            approx_tokens: 1,
            approx_bytes: 4,
            truncated: false,
        };
        assert_eq!(
            explain_step(&step),
            "tool_result Read (id=tu-2): ~1 tok / 4 bytes"
        );
    }
}
