//! lcov parsing (from `cargo llvm-cov --lcov`) and CRAP scores.
//!
//! CRAP (Change Risk Anti-Patterns) per function:
//! `crap = cc^2 * (1 - cov)^3 + cc`, where `cc` is the function's cyclomatic
//! complexity and `cov` its line coverage in [0, 1], joined by overlaying the
//! function's source line span onto the file's lcov `DA:` records.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::rust_metrics::FunctionMetrics;

#[derive(Debug, Default)]
pub struct Coverage {
    /// repo-relative path -> line -> hit count.
    files: HashMap<String, HashMap<usize, u64>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CrapScore {
    pub file: String,
    pub name: String,
    pub cyclomatic: u32,
    pub coverage_pct: f64,
    pub crap: f64,
}

impl Coverage {
    /// Parse an lcov tracefile. `repo_root` normalizes absolute `SF:` paths.
    pub fn from_lcov(path: &Path, repo_root: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading lcov file {}", path.display()))?;
        let mut cov = Coverage::default();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if let Some(sf) = line.strip_prefix("SF:") {
                let p = Path::new(sf);
                let rel = p
                    .strip_prefix(repo_root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/");
                current = Some(rel);
            } else if let Some(da) = line.strip_prefix("DA:") {
                if let Some(file) = &current {
                    let mut parts = da.splitn(2, ',');
                    let lineno: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
                    let hits: u64 = parts
                        .next()
                        .unwrap_or("0")
                        .split(',')
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    *cov.files
                        .entry(file.clone())
                        .or_default()
                        .entry(lineno)
                        .or_insert(0) += hits;
                }
            } else if line == "end_of_record" {
                current = None;
            }
        }
        Ok(cov)
    }

    /// Overall line coverage percentage across all instrumented files.
    pub fn total_line_coverage(&self) -> f64 {
        let (mut covered, mut total) = (0u64, 0u64);
        for lines in self.files.values() {
            for hits in lines.values() {
                total += 1;
                if *hits > 0 {
                    covered += 1;
                }
            }
        }
        if total == 0 {
            return 0.0;
        }
        covered as f64 / total as f64 * 100.0
    }

    pub fn instrumented_line_count(&self) -> u64 {
        self.files.values().map(|l| l.len() as u64).sum()
    }

    /// Line coverage for a span of a file; `None` when the file (or span)
    /// has no instrumented lines.
    fn span_coverage(&self, file: &str, start: usize, end: usize) -> Option<f64> {
        let lines = self.files.get(file)?;
        let (mut covered, mut total) = (0u64, 0u64);
        for (line, hits) in lines {
            if *line >= start && *line <= end {
                total += 1;
                if *hits > 0 {
                    covered += 1;
                }
            }
        }
        (total > 0).then(|| covered as f64 / total as f64)
    }

    /// CRAP score per analyzed function that has coverage data.
    pub fn crap_scores(&self, functions: &[FunctionMetrics]) -> Vec<CrapScore> {
        let mut out = Vec::new();
        for f in functions {
            let Some(cov) = self.span_coverage(&f.file, f.start_line, f.end_line) else {
                continue;
            };
            let cc = f.cyclomatic as f64;
            let crap = cc * cc * (1.0 - cov).powi(3) + cc;
            out.push(CrapScore {
                file: f.file.clone(),
                name: f.name.clone(),
                cyclomatic: f.cyclomatic,
                coverage_pct: cov * 100.0,
                crap,
            });
        }
        out.sort_by(|a, b| b.crap.total_cmp(&a.crap));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn lcov(content: &str) -> Coverage {
        let mut f = tempfile_path();
        f.1.write_all(content.as_bytes()).unwrap();
        Coverage::from_lcov(&f.0, Path::new("/repo")).unwrap()
    }

    fn tempfile_path() -> (std::path::PathBuf, std::fs::File) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lcov-test-{}-{n}.info", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        (path, file)
    }

    #[test]
    fn parses_da_records_and_totals() {
        let cov = lcov("SF:/repo/src/a.rs\nDA:1,5\nDA:2,0\nDA:3,1\nend_of_record\n");
        assert_eq!(cov.instrumented_line_count(), 3);
        let pct = cov.total_line_coverage();
        assert!((pct - 66.666).abs() < 0.1, "got {pct}");
    }

    #[test]
    fn crap_is_cc_at_full_coverage() {
        let cov = lcov("SF:/repo/src/a.rs\nDA:10,1\nDA:11,1\nend_of_record\n");
        let f = FunctionMetrics {
            file: "src/a.rs".into(),
            name: "f".into(),
            start_line: 9,
            end_line: 12,
            cyclomatic: 4,
            cognitive: 0,
            halstead_difficulty: 0.0,
        };
        let scores = cov.crap_scores(&[f]);
        assert_eq!(scores.len(), 1);
        assert!((scores[0].crap - 4.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_lcov_lines_do_not_end_a_record() {
        // FN:/FNDA:/LH: records must be ignored, not treated as terminators.
        let cov = lcov("SF:/repo/src/a.rs\nFN:1,foo\nDA:1,1\nLH:1\nend_of_record\n");
        assert_eq!(cov.instrumented_line_count(), 1);
        assert_eq!(cov.total_line_coverage(), 100.0);
    }

    #[test]
    fn span_coverage_is_inclusive_and_span_scoped() {
        // Lines 1 (hit) and 10 (missed) instrumented; the function spans
        // only line 1, so its coverage is 100% and CRAP equals cc.
        let cov = lcov("SF:/repo/src/a.rs\nDA:1,1\nDA:10,0\nend_of_record\n");
        let f = |start, end| FunctionMetrics {
            file: "src/a.rs".into(),
            name: "f".into(),
            start_line: start,
            end_line: end,
            cyclomatic: 2,
            cognitive: 0,
            halstead_difficulty: 0.0,
        };
        let scores = cov.crap_scores(&[f(1, 1)]);
        assert_eq!(scores[0].coverage_pct, 100.0);
        assert!((scores[0].crap - 2.0).abs() < 1e-9);
        // Inclusive end: span 2..=10 sees only the missed line.
        let scores = cov.crap_scores(&[f(2, 10)]);
        assert_eq!(scores[0].coverage_pct, 0.0);
    }

    #[test]
    fn functions_without_instrumented_lines_are_skipped() {
        let cov = lcov("SF:/repo/src/a.rs\nDA:1,1\nend_of_record\n");
        let f = FunctionMetrics {
            file: "src/a.rs".into(),
            name: "f".into(),
            start_line: 5,
            end_line: 9,
            cyclomatic: 2,
            cognitive: 0,
            halstead_difficulty: 0.0,
        };
        assert!(cov.crap_scores(&[f]).is_empty());
    }

    #[test]
    fn crap_formula_exact_at_half_coverage() {
        // cc = 3, coverage 2/4 = 0.5: crap = 9 * 0.125 + 3 = 4.125 exactly.
        let cov = lcov("SF:/repo/src/a.rs\nDA:1,1\nDA:2,1\nDA:3,0\nDA:4,0\nend_of_record\n");
        let f = FunctionMetrics {
            file: "src/a.rs".into(),
            name: "f".into(),
            start_line: 1,
            end_line: 4,
            cyclomatic: 3,
            cognitive: 0,
            halstead_difficulty: 0.0,
        };
        assert_eq!(cov.crap_scores(&[f])[0].crap, 4.125);
    }

    #[test]
    fn crap_penalizes_uncovered_complexity() {
        let cov = lcov("SF:/repo/src/a.rs\nDA:10,0\nend_of_record\n");
        let f = FunctionMetrics {
            file: "src/a.rs".into(),
            name: "f".into(),
            start_line: 10,
            end_line: 10,
            cyclomatic: 5,
            cognitive: 0,
            halstead_difficulty: 0.0,
        };
        // cc^2 * 1 + cc = 30
        assert!((cov.crap_scores(&[f])[0].crap - 30.0).abs() < 1e-9);
    }
}
