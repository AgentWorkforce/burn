//! Ghost-surface user-text loading for `hotspots --findings`: mines the
//! ledger's content sidecar for slash-command invocations that prompted the
//! selected turns.

use super::*;

pub(super) fn prompted_ghost_surface_inputs(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> crate::analyze::ghost_surface::GhostSurfaceInputs {
    let user_turn_text_by_session = load_ghost_surface_user_text_by_session(handle, turns);
    build_ghost_surface_inputs(turns, pricing, Some(user_turn_text_by_session))
}

/// User text that prompted the selected turns, keyed by source and session.
///
/// A prompt belongs to the first turn in its session at or after the
/// prompt's timestamp. Attribution runs against every turn in the session,
/// not only the selected ones, so a `since` window still sees the prompt
/// that triggered its first turn, and a partial-session filter (e.g. a
/// workflow stamp) never borrows prompts that drove unselected turns.
fn load_ghost_surface_user_text_by_session(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
) -> HashMap<SourceKind, HashMap<String, Vec<String>>> {
    let mut selected: HashMap<(SourceKind, &str), HashSet<&str>> = HashMap::new();
    for turn in turns {
        selected
            .entry((turn.source, turn.session_id.as_str()))
            .or_default()
            .insert(turn.message_id.as_str());
    }
    let mut out: HashMap<SourceKind, HashMap<String, Vec<String>>> = HashMap::new();
    for ((source, session_id), message_ids) in selected {
        let session_q = Query {
            session_id: Some(session_id.to_string()),
            source: Some(source),
            ..Default::default()
        };
        let (Ok(session_turns), Ok(records)) = (
            handle.inner.query_turns(&session_q),
            handle.inner.query_content(&session_q),
        ) else {
            continue;
        };
        let session_turns: Vec<TurnRecord> = session_turns
            .into_iter()
            .map(|enriched| enriched.turn)
            .filter(|turn| turn.session_id == session_id)
            .collect();
        let texts = prompts_for_selected_turns(&session_turns, &message_ids, records);
        if !texts.is_empty() {
            out.entry(source)
                .or_default()
                .insert(session_id.to_string(), texts);
        }
    }
    out
}

/// Non-empty user text records whose next turn (first turn with
/// `ts >= record.ts`) is one of `selected` message ids.
fn prompts_for_selected_turns(
    session_turns: &[TurnRecord],
    selected: &HashSet<&str>,
    records: Vec<ContentRecord>,
) -> Vec<String> {
    let mut timeline: Vec<(&str, &str)> = session_turns
        .iter()
        .map(|turn| (turn.ts.as_str(), turn.message_id.as_str()))
        .collect();
    timeline.sort_unstable();
    records
        .into_iter()
        .filter(|record| record.role == ContentRole::User && record.kind == ContentKind::Text)
        .filter(|record| {
            let next = timeline.partition_point(|(ts, _)| *ts < record.ts.as_str());
            timeline
                .get(next)
                .is_some_and(|(_, message_id)| selected.contains(message_id))
        })
        .filter_map(|record| record.text)
        .filter(|text| !text.is_empty())
        .collect()
}
