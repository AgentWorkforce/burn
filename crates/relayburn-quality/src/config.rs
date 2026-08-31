//! `quality.toml` — target thresholds plus a grandfathered baseline.
//!
//! `[targets]` holds the aspirational limits the benchmark reports against.
//! `[baseline]` lists existing violations (with their allowed ceiling) so the
//! CI gate passes today while forbidding *new* violations and regressions of
//! grandfathered ones. Shrink the baseline over time; never grow it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub targets: Targets,
    #[serde(default)]
    pub baseline: Baseline,
    pub sources: Sources,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    pub cyclomatic_max: u32,
    pub cognitive_max: u32,
    pub halstead_difficulty_max: f64,
    pub file_loc_max: usize,
    pub coverage_min_pct: f64,
    pub crap_max: f64,
    pub surviving_mutants_max: usize,
    pub dead_code_max: usize,
    pub redundant_code_max: usize,
    pub ts_any_max: usize,
    pub ts_unknown_max: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Enforced coverage floor while below the 100% target.
    pub coverage_min_pct: Option<f64>,
    /// Enforced ceiling for `unknown` usages in the TS surface.
    pub ts_unknown_max: Option<usize>,
    /// file path -> allowed LOC ceiling.
    #[serde(default)]
    pub file_loc: BTreeMap<String, usize>,
    /// "file::function" -> allowed cyclomatic ceiling.
    #[serde(default)]
    pub cyclomatic: BTreeMap<String, u32>,
    /// "file::function" -> allowed cognitive ceiling.
    #[serde(default)]
    pub cognitive: BTreeMap<String, u32>,
    /// "file::function" -> allowed Halstead-difficulty ceiling.
    #[serde(default)]
    pub halstead: BTreeMap<String, f64>,
    /// "file::function" -> allowed CRAP ceiling.
    #[serde(default)]
    pub crap: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// Roots scanned for production Rust code.
    pub rust_roots: Vec<String>,
    /// Roots scanned for TypeScript.
    pub ts_roots: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Key used for per-function baseline entries.
pub fn function_key(file: &str, name: &str) -> String {
    format!("{file}::{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_key_is_path_and_name() {
        assert_eq!(function_key("src/a.rs", "S::f"), "src/a.rs::S::f");
    }

    #[test]
    fn loads_the_repo_config() {
        // The checked-in quality.toml must always parse; baseline sections
        // are optional but targets and sources are required.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("quality.toml");
        let config = Config::load(&root).unwrap();
        assert!(config.targets.cyclomatic_max > 0);
        assert!(!config.sources.rust_roots.is_empty());
    }
}
