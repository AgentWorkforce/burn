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

/// Parse `mutants.out/outcomes.json` from cargo-mutants. The file carries
/// authoritative top-level counts (`total_mutants`, `missed`, `caught`,
/// `timeout`, `unviable`); the per-outcome list is only walked to name the
/// missed mutants.
pub fn parse_mutants_outcomes(path: &Path) -> Result<MutantCounts> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading mutants outcomes {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).context("parsing mutants outcomes JSON")?;
    let count = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut counts = MutantCounts {
        total: count("total_mutants"),
        caught: count("caught"),
        missed: count("missed"),
        timeout: count("timeout"),
        unviable: count("unviable"),
        missed_examples: Vec::new(),
    };
    let outcomes = v
        .get("outcomes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for o in &outcomes {
        if o.get("summary").and_then(Value::as_str) != Some("MissedMutant") {
            continue;
        }
        if counts.missed_examples.len() < 20 {
            if let Some(name) = o.pointer("/scenario/Mutant/name").and_then(Value::as_str) {
                counts.missed_examples.push(name.to_string());
            }
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

    fn msg(code: &str) -> String {
        format!(
            r#"{{"reason":"compiler-message","message":{{"code":{{"code":"{code}"}},"message":"m"}}}}"#
        )
    }

    #[test]
    fn each_lint_family_counts_and_gets_an_example() {
        let dead = ["dead_code", "unused_variables", "unreachable_code"];
        let redundant = [
            "clippy::redundant_closure",
            "clippy::needless_return",
            "clippy::duplicate_underscore_argument",
            "clippy::useless_conversion",
            "clippy::let_and_return",
        ];
        let log: String = dead
            .iter()
            .chain(redundant.iter())
            .chain(["clippy::unrelated_lint"].iter())
            .map(|c| msg(c) + "\n")
            .collect();
        let path = write_temp("clippy-families.json", &log);
        let counts = parse_clippy_log(&path).unwrap();
        assert_eq!(counts.dead_code, dead.len());
        assert_eq!(counts.redundant, redundant.len());
        // One example per flagged finding; unrelated lints add none.
        assert_eq!(counts.examples.len(), dead.len() + redundant.len());
    }

    #[test]
    fn example_lists_cap_at_twenty() {
        let log: String = (0..25).map(|_| msg("dead_code") + "\n").collect();
        let path = write_temp("clippy-cap.json", &log);
        let counts = parse_clippy_log(&path).unwrap();
        assert_eq!(counts.dead_code, 25);
        assert_eq!(counts.examples.len(), 20);

        let outcomes: Vec<String> = (0..25)
            .map(|i| {
                format!(r#"{{"summary":"MissedMutant","scenario":{{"Mutant":{{"name":"m{i}"}}}}}}"#)
            })
            .collect();
        let json = format!(
            r#"{{"total_mutants":25,"missed":25,"caught":0,"timeout":0,"unviable":0,"outcomes":[{}]}}"#,
            outcomes.join(",")
        );
        let path = write_temp("outcomes-cap.json", &json);
        let counts = parse_mutants_outcomes(&path).unwrap();
        assert_eq!(counts.missed, 25);
        assert_eq!(counts.missed_examples.len(), 20);
        assert_eq!(counts.missed_examples[0], "m0");
    }

    #[test]
    fn counts_missed_mutants() {
        let json = r#"{"total_mutants":2,"missed":1,"caught":1,"timeout":0,"unviable":0,
        "outcomes":[
            {"summary":"Success","scenario":"Baseline"},
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
