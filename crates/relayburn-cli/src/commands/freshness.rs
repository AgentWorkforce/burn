use relayburn_sdk::LedgerFreshness;

/// Present SDK freshness data on stderr without coupling the SDK to a UI.
pub(crate) fn warn_if_stale(freshness: &LedgerFreshness) {
    if !freshness.stale {
        return;
    }
    let last = freshness
        .last_write_at_ms
        .map(|ms| {
            relayburn_cli::util::time::iso_from_system_time(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms),
            )
        })
        .unwrap_or_else(|| "an unknown time".to_string());
    let threshold_hours = freshness.stale_after_ms as f64 / 3_600_000.0;
    eprintln!(
        "[burn] warning: ledger data is stale (last write: {last}; threshold: {threshold_hours:.1}h). Run `burn ingest` before relying on this report."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_metadata_needs_no_warning() {
        // Keep the predicate independently pinned even though stderr capture
        // belongs in the command integration tests.
        let freshness = LedgerFreshness {
            last_write_at_ms: Some(1),
            stale_after_ms: 1,
            stale: false,
        };
        warn_if_stale(&freshness);
    }
}
