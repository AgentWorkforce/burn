use super::*;

use std::collections::BTreeSet;

use crate::analyze::{cache_expiry_to_finding, cache_state_turns, detect_cache_expiry};

/// Cache-expiry findings for `turns`. Under a `since` bound, each session's
/// cache state at the window start comes from its own earlier turns, so a
/// resume inside the window is measured against the turn that last used the
/// cache. That lookup is per session and ignores provider, workflow, and tag
/// filters: cache state belongs to the session, not to the filtered slice.
pub(super) fn cache_expiry_findings(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    user_turns: &[UserTurnRecord],
    pricing: &PricingTable,
    q: &Query,
) -> Result<Vec<WasteFinding>> {
    let mut history: Vec<TurnRecord> = Vec::new();
    if let Some(since) = q.since.as_deref() {
        let sessions: BTreeSet<&str> = turns.iter().map(|t| t.session_id.as_str()).collect();
        for session_id in sessions {
            let before_window = Query {
                session_id: Some(session_id.to_string()),
                until: Some(since.to_string()),
                ..Default::default()
            };
            let earlier: Vec<TurnRecord> = collect_turns(handle, &before_window)?
                .into_iter()
                .filter(|t| t.ts.as_str() < since)
                .collect();
            history.extend(cache_state_turns(&earlier));
        }
    }
    history.extend_from_slice(turns);
    Ok(
        detect_cache_expiry(&history, user_turns, pricing, q.since.as_deref())
            .iter()
            .map(cache_expiry_to_finding)
            .collect(),
    )
}
