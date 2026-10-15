use super::*;

use std::collections::BTreeSet;

use crate::analyze::{cache_state_turns, turn_id};

/// The turns before `q.since` that carry each session's cache state into the
/// window, so a resume inside the window is measured against the turn that
/// last used the cache. The lookup is per session and ignores provider,
/// workflow, and tag filters: cache state belongs to the session, not to the
/// filtered slice. Empty without a `since` bound.
pub(super) fn cache_state_before_window(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    q: &Query,
) -> Result<Vec<TurnRecord>> {
    let Some(since) = q.since.as_deref() else {
        return Ok(Vec::new());
    };
    let in_window: HashSet<_> = turns.iter().map(turn_id).collect();
    let sessions: BTreeSet<&str> = turns.iter().map(|t| t.session_id.as_str()).collect();
    let mut history = Vec::new();
    for session_id in sessions {
        let mut before_window = build_query(Some(session_id), None, None)?;
        before_window.until = Some(since.to_string());
        let earlier: Vec<TurnRecord> = collect_turns(handle, &before_window)?
            .into_iter()
            .filter(|t| !in_window.contains(&turn_id(t)))
            .collect();
        history.extend(cache_state_turns(&earlier));
    }
    Ok(history)
}

#[cfg(test)]
#[path = "cache_expiry_tests.rs"]
mod tests;
