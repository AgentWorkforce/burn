use super::*;

use std::collections::BTreeSet;

use crate::analyze::{cache_expiry_to_finding, cache_state_turns, detect_cache_expiry, turn_id};

/// Cache-expiry findings for `turns`. Under a `since` bound, each session's
/// cache state at the window start comes from its own turns up to `since`,
/// so a resume inside the window is measured against the turn that last used
/// the cache. That lookup is per session and ignores provider, workflow, and
/// tag filters: cache state belongs to the session, not to the filtered
/// slice. Only turns in `turns` are reported.
pub(super) fn cache_expiry_findings(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    user_turns: &[UserTurnRecord],
    pricing: &PricingTable,
    q: &Query,
) -> Result<Vec<WasteFinding>> {
    let in_window: HashSet<_> = turns.iter().map(turn_id).collect();
    let mut history = turns.to_vec();
    if let Some(since) = q.since.as_deref() {
        let sessions: BTreeSet<&str> = turns.iter().map(|t| t.session_id.as_str()).collect();
        for session_id in sessions {
            let mut before_window = build_query(Some(session_id), None, None)?;
            before_window.until = Some(since.to_string());
            let earlier: Vec<TurnRecord> = collect_turns(handle, &before_window)?
                .into_iter()
                .filter(|t| !in_window.contains(&turn_id(t)))
                .collect();
            history.extend(cache_state_turns(&earlier));
        }
    }
    Ok(
        detect_cache_expiry(&history, user_turns, pricing, Some(&in_window))
            .iter()
            .map(cache_expiry_to_finding)
            .collect(),
    )
}

#[cfg(test)]
#[path = "cache_expiry_tests.rs"]
mod tests;
