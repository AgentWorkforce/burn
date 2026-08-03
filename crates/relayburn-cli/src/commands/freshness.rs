use relayburn_sdk::LedgerFreshness;

use crate::cli::GlobalArgs;

/// Present SDK freshness data on stderr without coupling the SDK to a UI.
pub(crate) fn warn_if_stale(freshness: &LedgerFreshness, globals: &GlobalArgs) {
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
        .unwrap_or_else(|| "never".to_string());
    let threshold_hours = freshness.stale_after_ms.unwrap_or_default() as f64 / 3_600_000.0;
    crate::render::ux::print_warning(
        &format!(
            "ledger data may be stale (last write: {last}; threshold: {threshold_hours:.1}h). If expected activity is missing, run `burn ingest` before relying on this report."
        ),
        globals,
    );
}
