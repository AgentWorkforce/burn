//! `burn-quality` — the relayburn code-quality benchmark.
//!
//! Measures the workspace against the quality targets in `quality.toml`
//! (complexity, Halstead difficulty, file size, coverage, CRAP, mutants,
//! dead/redundant code, TS `any`/`unknown`) and exits non-zero when the
//! enforced gate fails. See `quality.toml` for the target-vs-baseline model.

mod complexity;
mod config;
mod coverage;
mod external;
mod loc;
mod report;
mod rust_metrics;
mod ts_metrics;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "burn-quality",
    about = "Code-quality benchmark for the relayburn workspace"
)]
struct Args {
    /// Path to quality.toml (defaults to the repo root copy).
    #[arg(long, default_value = "quality.toml")]
    config: PathBuf,

    /// lcov tracefile from `cargo llvm-cov --lcov` (enables coverage + CRAP).
    #[arg(long)]
    coverage: Option<PathBuf>,

    /// JSON log from `cargo clippy --message-format=json` (enables dead /
    /// redundant code counts).
    #[arg(long)]
    clippy_log: Option<PathBuf>,

    /// `mutants.out/outcomes.json` from cargo-mutants (enables the surviving
    /// mutants count).
    #[arg(long)]
    mutants: Option<PathBuf>,

    /// Write the full report as JSON to this path.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Print a regenerated `[baseline]` TOML section for every current
    /// violation of the targets, then exit 0 without gating.
    #[arg(long)]
    write_baseline: bool,

    /// Append a markdown summary to this path (e.g. $GITHUB_STEP_SUMMARY).
    #[arg(long)]
    github_summary: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let repo_root = std::env::current_dir().context("resolving working directory")?;
    let config = config::Config::load(&repo_root.join(&args.config))?;

    let rust = rust_metrics::collect(&repo_root, &config.sources.rust_roots)?;
    let ts = ts_metrics::collect(&repo_root, &config.sources.ts_roots)?;

    let cov = args
        .coverage
        .as_deref()
        .map(|p| coverage::Coverage::from_lcov(p, &repo_root))
        .transpose()?;
    let coverage_pct = cov.as_ref().map(coverage::Coverage::total_line_coverage);
    let crap = cov.as_ref().map(|c| c.crap_scores(&rust.functions));

    let lints = args
        .clippy_log
        .as_deref()
        .map(external::parse_clippy_log)
        .transpose()?;
    let mutants = args
        .mutants
        .as_deref()
        .map(external::parse_mutants_outcomes)
        .transpose()?;

    if args.write_baseline {
        print!(
            "{}",
            baseline_toml(&config, &rust, &ts, coverage_pct, crap.as_deref())
        );
        return Ok(());
    }

    let report = report::build(
        &config,
        &rust,
        &ts,
        coverage_pct,
        crap.as_deref(),
        lints.as_ref(),
        mutants.as_ref(),
    );

    if let Some(c) = &cov {
        println!(
            "coverage input: {} instrumented lines\n",
            c.instrumented_line_count()
        );
    }
    println!("{}", report::render_table(&report));
    if !report.violations.is_empty() {
        eprintln!("quality gate violations:");
        for v in &report.violations {
            eprintln!("  - {v}");
        }
    }

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(path) = &args.github_summary {
        let mut md = String::from("### Code quality benchmark\n\n");
        md.push_str("| Metric | Value | Target | Target met | Gate |\n|---|---|---|---|---|\n");
        for row in &report.rows {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                row.metric,
                row.value,
                row.target,
                if row.meets_target { "✅" } else { "⚠️" },
                if row.passes_gate { "✅" } else { "❌" },
            ));
        }
        if !report.violations.is_empty() {
            md.push_str("\n**Violations:**\n");
            for v in &report.violations {
                md.push_str(&format!("- {v}\n"));
            }
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.write_all(md.as_bytes())?;
    }

    if !report.gate_passed() {
        std::process::exit(1);
    }
    Ok(())
}

/// Render a `[baseline]` section covering every current target violation.
fn baseline_toml(
    config: &config::Config,
    rust: &rust_metrics::RustMetrics,
    ts: &ts_metrics::TsTypeCounts,
    coverage_pct: Option<f64>,
    crap: Option<&[coverage::CrapScore]>,
) -> String {
    use std::fmt::Write;
    let t = &config.targets;
    let mut out = String::from("[baseline]\n");
    if let Some(pct) = coverage_pct {
        if pct < t.coverage_min_pct {
            // Floor slightly below the measured value so unrelated PRs don't
            // flap on fractional coverage noise.
            let _ = writeln!(out, "coverage_min_pct = {:.1}", (pct - 0.5).max(0.0));
        }
    }
    if ts.unknown_count > t.ts_unknown_max {
        let _ = writeln!(out, "ts_unknown_max = {}", ts.unknown_count);
    }

    let mut section = |name: &str, entries: Vec<(String, String)>| {
        if entries.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n[baseline.{name}]");
        for (k, v) in entries {
            let _ = writeln!(out, "\"{k}\" = {v}");
        }
    };

    section(
        "file_loc",
        rust.files
            .iter()
            .filter(|f| f.loc > t.file_loc_max)
            .map(|f| (f.file.clone(), f.loc.to_string()))
            .collect(),
    );
    section(
        "cyclomatic",
        rust.functions
            .iter()
            .filter(|f| f.cyclomatic > t.cyclomatic_max)
            .map(|f| {
                (
                    config::function_key(&f.file, &f.name),
                    f.cyclomatic.to_string(),
                )
            })
            .collect(),
    );
    section(
        "cognitive",
        rust.functions
            .iter()
            .filter(|f| f.cognitive > t.cognitive_max)
            .map(|f| {
                (
                    config::function_key(&f.file, &f.name),
                    f.cognitive.to_string(),
                )
            })
            .collect(),
    );
    section(
        "halstead",
        rust.functions
            .iter()
            .filter(|f| f.halstead_difficulty > t.halstead_difficulty_max)
            .map(|f| {
                (
                    config::function_key(&f.file, &f.name),
                    format!("{:.1}", f.halstead_difficulty + 0.1),
                )
            })
            .collect(),
    );
    if let Some(scores) = crap {
        section(
            "crap",
            scores
                .iter()
                .filter(|s| s.crap > t.crap_max)
                .map(|s| {
                    (
                        config::function_key(&s.file, &s.name),
                        format!("{:.1}", s.crap + 0.1),
                    )
                })
                .collect(),
        );
    }
    out
}
