//! Gate evaluation and rendering: compares measured values against targets
//! and the grandfathered baseline, prints the benchmark table, and yields the
//! process exit status.

use std::collections::BTreeMap;

use crate::config::{function_key, Config};
use crate::coverage::CrapScore;
use crate::external::{LintCounts, MutantCounts};
use crate::rust_metrics::RustMetrics;
use crate::ts_metrics::TsTypeCounts;

#[derive(Debug, serde::Serialize)]
pub struct MetricRow {
    pub metric: String,
    pub value: String,
    pub target: String,
    /// Meets the aspirational target.
    pub meets_target: bool,
    /// Passes the enforced gate (target, or grandfathered baseline).
    pub passes_gate: bool,
    pub detail: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub rows: Vec<MetricRow>,
    pub violations: Vec<String>,
}

impl Report {
    pub fn gate_passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// Row that failed the enforced gate: any violation line carries the
    /// metric's key prefix.
    fn push_row(&mut self, key: &str, mut row: MetricRow) {
        row.passes_gate = !self
            .violations
            .iter()
            .any(|v| v.starts_with(&format!("{key}:")));
        self.rows.push(row);
    }
}

/// Worst offenders over a per-item limit, honoring baseline allowances.
/// Returns (worst value, count over target).
fn check_items<V: PartialOrd + Copy + std::fmt::Display>(
    items: impl Iterator<Item = (String, V)>,
    target: V,
    baseline: &BTreeMap<String, V>,
    metric: &str,
    violations: &mut Vec<String>,
) -> (Option<(String, V)>, usize) {
    let mut worst: Option<(String, V)> = None;
    let mut over_target = 0;
    for (key, value) in items {
        if worst.as_ref().is_none_or(|(_, w)| value > *w) {
            worst = Some((key.clone(), value));
        }
        if value > target {
            over_target += 1;
            match baseline.get(&key) {
                Some(allowed) if value <= *allowed => {}
                Some(allowed) => violations.push(format!(
                    "{metric}: {key} = {value} exceeds its grandfathered ceiling {allowed}"
                )),
                None => violations.push(format!(
                    "{metric}: {key} = {value} exceeds target {target} (new violation)"
                )),
            }
        }
    }
    (worst, over_target)
}

pub fn build(
    config: &Config,
    rust: &RustMetrics,
    ts: &TsTypeCounts,
    coverage_pct: Option<f64>,
    crap: Option<&[CrapScore]>,
    lints: Option<&LintCounts>,
    mutants: Option<&MutantCounts>,
) -> Report {
    let mut report = Report::default();
    source_rows(config, rust, &mut report);
    coverage_row(config, coverage_pct, &mut report);
    crap_row(config, crap, &mut report);
    mutants_row(config, mutants, &mut report);
    lint_rows(config, lints, &mut report);
    ts_rows(config, ts, &mut report);
    report
}

/// Add a per-item metric row (worst value + violation accounting).
fn item_metric_row<V: PartialOrd + Copy + std::fmt::Display>(
    report: &mut Report,
    key: &str,
    label: &str,
    items: impl Iterator<Item = (String, V)>,
    target: V,
    baseline: &BTreeMap<String, V>,
) {
    let (worst, over) = check_items(items, target, baseline, key, &mut report.violations);
    let worst_s = worst.map(|(k, v)| (k, v.to_string()));
    let detail = match &worst_s {
        Some((k, v)) => format!("{over} over target; worst: {k} = {v}"),
        None => String::new(),
    };
    report.push_row(
        key,
        MetricRow {
            metric: label.into(),
            value: worst_s.map(|(_, v)| v).unwrap_or_default(),
            target: format!("<= {target}"),
            meets_target: over == 0,
            passes_gate: true,
            detail,
        },
    );
}

fn source_rows(config: &Config, rust: &RustMetrics, report: &mut Report) {
    let t = &config.targets;
    let b = &config.baseline;
    item_metric_row(
        report,
        "cyclomatic",
        "Cyclomatic complexity (per fn)",
        rust.functions
            .iter()
            .map(|f| (function_key(&f.file, &f.name), f.cyclomatic)),
        t.cyclomatic_max,
        &b.cyclomatic,
    );
    item_metric_row(
        report,
        "cognitive",
        "Cognitive complexity (per fn)",
        rust.functions
            .iter()
            .map(|f| (function_key(&f.file, &f.name), f.cognitive)),
        t.cognitive_max,
        &b.cognitive,
    );
    item_metric_row(
        report,
        "halstead",
        "Halstead difficulty (per fn)",
        rust.functions.iter().map(|f| {
            (
                function_key(&f.file, &f.name),
                Rounded(f.halstead_difficulty),
            )
        }),
        Rounded(t.halstead_difficulty_max),
        &b.halstead
            .iter()
            .map(|(k, v)| (k.clone(), Rounded(*v)))
            .collect(),
    );
    item_metric_row(
        report,
        "file_loc",
        "Lines of code per file",
        rust.files.iter().map(|f| (f.file.clone(), f.loc)),
        t.file_loc_max,
        &b.file_loc,
    );
}

fn coverage_row(config: &Config, coverage_pct: Option<f64>, report: &mut Report) {
    let t = &config.targets;
    let Some(pct) = coverage_pct else {
        report.rows.push(not_measured(
            "Test coverage (line)",
            &format!("{}%", t.coverage_min_pct),
            "run with --coverage lcov.info",
        ));
        return;
    };
    let floor = config
        .baseline
        .coverage_min_pct
        .unwrap_or(t.coverage_min_pct);
    if pct + 1e-9 < floor {
        report.violations.push(format!(
            "coverage: {pct:.2}% is below the enforced floor {floor:.2}%"
        ));
    }
    report.push_row(
        "coverage",
        MetricRow {
            metric: "Test coverage (line)".into(),
            value: format!("{pct:.2}%"),
            target: format!("{}%", t.coverage_min_pct),
            meets_target: pct + 1e-9 >= t.coverage_min_pct,
            passes_gate: true,
            detail: format!("enforced floor: {floor:.2}%"),
        },
    );
}

fn crap_row(config: &Config, crap: Option<&[CrapScore]>, report: &mut Report) {
    let t = &config.targets;
    let Some(scores) = crap else {
        report.rows.push(not_measured(
            "CRAP score (per fn)",
            &format!("<= {}", t.crap_max),
            "needs --coverage",
        ));
        return;
    };
    item_metric_row(
        report,
        "crap",
        "CRAP score (per fn)",
        scores
            .iter()
            .map(|s| (function_key(&s.file, &s.name), Rounded(s.crap))),
        Rounded(t.crap_max),
        &config
            .baseline
            .crap
            .iter()
            .map(|(k, v)| (k.clone(), Rounded(*v)))
            .collect(),
    );
}

fn mutants_row(config: &Config, mutants: Option<&MutantCounts>, report: &mut Report) {
    let t = &config.targets;
    let Some(m) = mutants else {
        report.rows.push(not_measured(
            "Surviving mutants",
            &format!("<= {}", t.surviving_mutants_max),
            "run cargo-mutants and pass --mutants outcomes.json",
        ));
        return;
    };
    if m.missed > t.surviving_mutants_max {
        report.violations.push(format!(
            "mutants: {} surviving mutants (max {}): {}",
            m.missed,
            t.surviving_mutants_max,
            m.missed_examples.join("; ")
        ));
    }
    report.push_row(
        "mutants",
        MetricRow {
            metric: "Surviving mutants".into(),
            value: m.missed.to_string(),
            target: format!("<= {}", t.surviving_mutants_max),
            meets_target: m.missed <= t.surviving_mutants_max,
            passes_gate: true,
            detail: format!(
                "{} tested, {} caught, {} unviable",
                m.total, m.caught, m.unviable
            ),
        },
    );
}

fn lint_rows(config: &Config, lints: Option<&LintCounts>, report: &mut Report) {
    let t = &config.targets;
    let Some(l) = lints else {
        for name in ["Dead code findings", "Redundant code findings"] {
            report
                .rows
                .push(not_measured(name, "<= 0", "needs --clippy-log"));
        }
        return;
    };
    if l.dead_code > t.dead_code_max {
        report.violations.push(format!(
            "dead code: {} findings (max {})",
            l.dead_code, t.dead_code_max
        ));
    }
    report.push_row(
        "dead code",
        MetricRow {
            metric: "Dead code findings".into(),
            value: l.dead_code.to_string(),
            target: format!("<= {}", t.dead_code_max),
            meets_target: l.dead_code <= t.dead_code_max,
            passes_gate: true,
            detail: "rustc dead/unused/unreachable lints".into(),
        },
    );
    if l.redundant > t.redundant_code_max {
        report.violations.push(format!(
            "redundant code: {} findings (max {})",
            l.redundant, t.redundant_code_max
        ));
    }
    report.push_row(
        "redundant code",
        MetricRow {
            metric: "Redundant code findings".into(),
            value: l.redundant.to_string(),
            target: format!("<= {}", t.redundant_code_max),
            meets_target: l.redundant <= t.redundant_code_max,
            passes_gate: true,
            detail: "clippy redundant/needless/duplicate lints".into(),
        },
    );
}

fn ts_rows(config: &Config, ts: &TsTypeCounts, report: &mut Report) {
    let t = &config.targets;
    if ts.any_count > t.ts_any_max {
        report.violations.push(format!(
            "ts any: {} usages (max {})",
            ts.any_count, t.ts_any_max
        ));
    }
    report.push_row(
        "ts any",
        MetricRow {
            metric: "TS `any` types".into(),
            value: ts.any_count.to_string(),
            target: format!("<= {}", t.ts_any_max),
            meets_target: ts.any_count <= t.ts_any_max,
            passes_gate: true,
            detail: String::new(),
        },
    );
    let unknown_ceiling = config.baseline.ts_unknown_max.unwrap_or(t.ts_unknown_max);
    if ts.unknown_count > unknown_ceiling {
        report.violations.push(format!(
            "ts unknown: {} usages exceed the enforced ceiling {}",
            ts.unknown_count, unknown_ceiling
        ));
    }
    report.push_row(
        "ts unknown",
        MetricRow {
            metric: "TS `unknown` types".into(),
            value: ts.unknown_count.to_string(),
            target: format!("<= {}", t.ts_unknown_max),
            meets_target: ts.unknown_count <= t.ts_unknown_max,
            passes_gate: true,
            detail: format!("enforced ceiling: {unknown_ceiling}"),
        },
    );
}

fn not_measured(metric: &str, target: &str, detail: &str) -> MetricRow {
    MetricRow {
        metric: metric.into(),
        value: "not measured".into(),
        target: target.into(),
        meets_target: false,
        passes_gate: true,
        detail: detail.into(),
    }
}

/// f64 wrapper displayed with one decimal so baselines compare stably.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub struct Rounded(pub f64);

impl std::fmt::Display for Rounded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}", self.0)
    }
}

pub fn render_table(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<34} {:>14} {:>10}  {:^6} {:^6}  {}\n",
        "metric", "value", "target", "target", "gate", "detail"
    ));
    out.push_str(&"-".repeat(110));
    out.push('\n');
    for row in &report.rows {
        out.push_str(&format!(
            "{:<34} {:>14} {:>10}  {:^6} {:^6}  {}\n",
            row.metric,
            row.value,
            row.target,
            if row.meets_target { "PASS" } else { "MISS" },
            if row.passes_gate { "PASS" } else { "FAIL" },
            row.detail,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn baseline_allows_grandfathered_violation() {
        let mut baseline = BTreeMap::new();
        baseline.insert("a.rs::big".to_string(), 30u32);
        let mut violations = Vec::new();
        let items = vec![
            ("a.rs::big".to_string(), 28u32),
            ("a.rs::ok".to_string(), 3u32),
        ];
        let (worst, over) = check_items(
            items.into_iter(),
            22,
            &baseline,
            "cyclomatic",
            &mut violations,
        );
        assert_eq!(over, 1);
        assert_eq!(worst.unwrap().1, 28);
        assert!(violations.is_empty());
    }

    #[test]
    fn new_violation_fails_gate() {
        let mut violations = Vec::new();
        let items = vec![("a.rs::new_big".to_string(), 25u32)];
        check_items(
            items.into_iter(),
            22,
            &BTreeMap::new(),
            "cyclomatic",
            &mut violations,
        );
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("new violation"));
    }

    #[test]
    fn regression_beyond_ceiling_fails_gate() {
        let mut baseline = BTreeMap::new();
        baseline.insert("a.rs::big".to_string(), 25u32);
        let mut violations = Vec::new();
        let items = vec![("a.rs::big".to_string(), 26u32)];
        check_items(
            items.into_iter(),
            22,
            &baseline,
            "cyclomatic",
            &mut violations,
        );
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("grandfathered ceiling"));
    }

    #[test]
    fn push_row_marks_gate_failures() {
        let mut report = Report::default();
        report
            .violations
            .push("coverage: 50% is below the enforced floor 60%".into());
        report.push_row(
            "coverage",
            MetricRow {
                metric: "Test coverage (line)".into(),
                value: "50%".into(),
                target: "100%".into(),
                meets_target: false,
                passes_gate: true,
                detail: String::new(),
            },
        );
        assert!(!report.rows[0].passes_gate);
    }
}
