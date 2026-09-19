//! Human-output helpers for unknown model pricing.
//!
//! Query verbs load tariffs from the override adjacent to the open ledger
//! (`burn_path().parent()/models.dev.json`). Presenters print that same
//! resolved path when they warn about unpriced turns.

use std::path::{Path, PathBuf};

use relayburn_sdk::LedgerHandle;

/// Active `models.dev.json` override path for this ledger, as an absolute
/// path. Matches `load_pricing_for_ledger`: the query layer prices from
/// `burn_path().parent()`, not from a `$RELAYBURN_HOME` placeholder.
pub fn pricing_override_path(handle: &LedgerHandle) -> PathBuf {
    pricing_override_path_from_burn_path(handle.raw().burn_path())
}

pub fn pricing_override_path_from_burn_path(burn_path: &Path) -> PathBuf {
    let path = burn_path
        .parent()
        .map(|home| home.join("models.dev.json"))
        .unwrap_or_else(|| PathBuf::from("models.dev.json"));
    std::path::absolute(&path).unwrap_or(path)
}

/// Stderr warning used by `summary` and `compare` human output when the
/// report includes turns whose model has no pricing entry.
pub fn warn_unpriced_usage(unpriced_turns: u64, unpriced_models: &[String], override_path: &Path) {
    if let Some((first, second)) =
        unpriced_warning_lines(unpriced_turns, unpriced_models, override_path)
    {
        eprintln!("{first}");
        eprintln!("{second}");
    }
}

pub fn unpriced_warning_lines(
    unpriced_turns: u64,
    unpriced_models: &[String],
    override_path: &Path,
) -> Option<(String, String)> {
    if unpriced_turns == 0 {
        return None;
    }
    let first = if unpriced_models.is_empty() {
        format!("warning: {unpriced_turns} turn(s) had no pricing.")
    } else {
        format!(
            "warning: {unpriced_turns} turn(s) had no pricing for model(s): {}.",
            unpriced_models.join(", "),
        )
    };
    let second = format!(
        "         Update the snapshot (pnpm run pricing:update) or add an override at {}.",
        override_path.display(),
    );
    Some((first, second))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn override_path_is_absolute_and_adjacent_to_ledger() {
        let path = pricing_override_path_from_burn_path(Path::new("/tmp/ledger/burn.sqlite"));
        assert_eq!(path, PathBuf::from("/tmp/ledger/models.dev.json"));
    }

    #[test]
    fn relative_override_path_resolves_against_cwd() {
        let path = pricing_override_path_from_burn_path(Path::new("nested/burn.sqlite"));
        assert!(path.is_absolute(), "{path:?}");
        assert!(path.ends_with("nested/models.dev.json"), "{path:?}");
    }

    #[test]
    fn warning_prints_resolved_override_path() {
        let (first, second) = unpriced_warning_lines(
            2,
            &["future-model".into()],
            Path::new("/abs/home/models.dev.json"),
        )
        .expect("unpriced warning");
        assert_eq!(
            first,
            "warning: 2 turn(s) had no pricing for model(s): future-model."
        );
        assert_eq!(
            second,
            "         Update the snapshot (pnpm run pricing:update) or add an override at /abs/home/models.dev.json."
        );
    }

    #[test]
    fn warning_omitted_when_all_turns_priced() {
        assert!(unpriced_warning_lines(0, &[], Path::new("/x/models.dev.json")).is_none());
    }
}
