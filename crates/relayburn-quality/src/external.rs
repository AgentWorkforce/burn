//! Parsers for externally produced inputs: a `cargo clippy
//! --message-format=json` log (dead code + redundant code counts) and a
//! `cargo mutants` `outcomes.json` (surviving mutants).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LintCounts {
    pub dead_code: usize,
    pub redundant: usize,
    pub examples: Vec<String>,
}

/// Lints that indicate unreachable / unused ("dead") code.
const DEAD_CODE_LINTS: &[&str] = &[
    "dead_code",
    "unused_variables",
    "unused_imports",
    "unused_mut",
    "unreachable_code",
    "unreachable_patterns",
    "unused_must_use",
    "unused_assignments",
];

/// Count dead-code and redundant-code diagnostics from a clippy JSON log.
/// Redundant code is any `clippy::redundant_*` / `clippy::needless_*` /
/// `clippy::duplicate*` lint.
pub fn parse_clippy_log(path: &Path) -> Result<LintCounts> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading clippy log {}", path.display()))?;
    let mut counts = LintCounts::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(code) = v
            .pointer("/message/code/code")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let short = v
            .pointer("/message/message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_dead = DEAD_CODE_LINTS.contains(&code.as_str());
        let is_redundant = code.starts_with("clippy::redundant")
            || code.starts_with("clippy::needless")
            || code.starts_with("clippy::duplicate")
            || code == "clippy::useless_conversion"
            || code == "clippy::let_and_return";
        if is_dead {
            counts.dead_code += 1;
        }
        if is_redundant {
            counts.redundant += 1;
        }
        if (is_dead || is_redundant) && counts.examples.len() < 20 {
            counts.examples.push(format!("{code}: {short}"));
        }
    }
    Ok(counts)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MutantCounts {
    pub total: usize,
    pub caught: usize,
    pub missed: usize,
    pub timeout: usize,
    pub unviable: usize,
    pub missed_examples: Vec<String>,
}

/// Parse `mutants.out/outcomes.json` from cargo-mutants.
pub fn parse_mutants_outcomes(path: &Path) -> Result<MutantCounts> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading mutants outcomes {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).context("parsing mutants outcomes JSON")?;
    let mut counts = MutantCounts::default();
    let outcomes = v
        .get("outcomes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for o in &outcomes {
        let summary = o.get("summary").and_then(Value::as_str).unwrap_or("");
        counts.total += 1;
        match summary {
            "CaughtMutant" => counts.caught += 1,
            "MissedMutant" => {
                counts.missed += 1;
                if counts.missed_examples.len() < 20 {
                    if let Some(name) = o.pointer("/scenario/Mutant/name").and_then(Value::as_str) {
                        counts.missed_examples.push(name.to_string());
                    }
                }
            }
            "Timeout" => counts.timeout += 1,
            "Unviable" => counts.unviable += 1,
            _ => {}
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("quality-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn counts_dead_and_redundant_lints() {
        let log = concat!(
            r#"{"reason":"compiler-message","message":{"code":{"code":"dead_code"},"message":"unused fn"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":{"code":"clippy::redundant_clone"},"message":"redundant clone"}}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let path = write_temp("clippy.json", log);
        let counts = parse_clippy_log(&path).unwrap();
        assert_eq!(counts.dead_code, 1);
        assert_eq!(counts.redundant, 1);
    }

    #[test]
    fn counts_missed_mutants() {
        let json = r#"{"outcomes":[
            {"summary":"CaughtMutant","scenario":{"Mutant":{"name":"a"}}},
            {"summary":"MissedMutant","scenario":{"Mutant":{"name":"replace foo -> bar"}}}
        ]}"#;
        let path = write_temp("outcomes.json", json);
        let counts = parse_mutants_outcomes(&path).unwrap();
        assert_eq!(counts.total, 2);
        assert_eq!(counts.caught, 1);
        assert_eq!(counts.missed, 1);
        assert_eq!(counts.missed_examples, vec!["replace foo -> bar"]);
    }
}
