//! Finding kinds `hotspots` evaluates when the caller names no patterns.

const DEFAULT_HOTSPOTS_FINDING_KINDS: &[&str] = &[
    "context-output-ratio",
    "retry-loop",
    "failure-run",
    "cancellation-run",
    "compaction-loss",
    "edit-revert",
    "edit-heavy",
    "skill-recall-dup",
    "skill-pruning-protection",
    "system-prompt-tax",
    "ghost-surface",
    "tool-output-bloat",
    "tool-call-pattern",
    "cache-expiry",
    "unpriced-usage",
];

pub(super) fn default_hotspots_finding_kinds() -> Vec<String> {
    DEFAULT_HOTSPOTS_FINDING_KINDS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kinds_list_every_detector_in_evaluation_order() {
        assert_eq!(
            default_hotspots_finding_kinds(),
            vec![
                "context-output-ratio",
                "retry-loop",
                "failure-run",
                "cancellation-run",
                "compaction-loss",
                "edit-revert",
                "edit-heavy",
                "skill-recall-dup",
                "skill-pruning-protection",
                "system-prompt-tax",
                "ghost-surface",
                "tool-output-bloat",
                "tool-call-pattern",
                "cache-expiry",
                "unpriced-usage",
            ]
        );
    }
}
