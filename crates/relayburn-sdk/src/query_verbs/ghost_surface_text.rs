//! Ghost-surface user-text loading for `hotspots --findings`: mines the
//! ledger's content sidecar for slash-command invocations that prompted the
//! selected turns.

use super::*;

pub(super) fn prompted_ghost_surface_inputs(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Result<crate::analyze::ghost_surface::GhostSurfaceInputs> {
    let user_turn_text_by_session = load_ghost_surface_user_text_by_session(handle, turns)?;
    Ok(build_ghost_surface_inputs(
        turns,
        pricing,
        Some(user_turn_text_by_session),
    ))
}

/// User text that prompted `turns`, from the content records of a complete
/// session read, keyed by source and session as the ghost-surface detector
/// reads it. Every turn counts as selected.
pub(crate) fn session_prompt_text(
    turns: &[TurnRecord],
    content: &[ContentRecord],
) -> HashMap<SourceKind, HashMap<String, Vec<String>>> {
    let mut sessions: HashMap<(SourceKind, &str), Vec<TurnRecord>> = HashMap::new();
    for turn in turns {
        sessions
            .entry((turn.source, turn.session_id.as_str()))
            .or_default()
            .push(turn.clone());
    }
    let mut out: HashMap<SourceKind, HashMap<String, Vec<String>>> = HashMap::new();
    for ((source, session_id), session_turns) in sessions {
        let selected: HashSet<&str> = session_turns
            .iter()
            .map(|t| t.message_id.as_str())
            .collect();
        let records = content
            .iter()
            .filter(|record| record.session_id == session_id)
            .cloned()
            .collect();
        let texts = prompts_for_selected_turns(&session_turns, &selected, records);
        if !texts.is_empty() {
            out.entry(source)
                .or_default()
                .insert(session_id.to_string(), texts);
        }
    }
    out
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
) -> Result<HashMap<SourceKind, HashMap<String, Vec<String>>>> {
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
        let records = handle.inner.query_content(&session_q)?;
        let session_turns: Vec<TurnRecord> = handle
            .inner
            .query_turns(&session_q)?
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
    Ok(out)
}

/// Non-empty user text records whose next turn (first turn at or after the
/// record's instant) is one of `selected` message ids. Timestamps compare
/// as instants, so `…:10Z` and `…:10.000Z` are the same moment; one that
/// does not parse sorts before every instant.
fn prompts_for_selected_turns(
    session_turns: &[TurnRecord],
    selected: &HashSet<&str>,
    records: Vec<ContentRecord>,
) -> Vec<String> {
    let instant = |ts: &str| crate::util::time::parse_iso_ms(ts).unwrap_or(i64::MIN);
    let mut timeline: Vec<(i64, &str)> = session_turns
        .iter()
        .map(|turn| (instant(&turn.ts), turn.message_id.as_str()))
        .collect();
    timeline.sort_unstable();
    records
        .into_iter()
        .filter(|record| record.role == ContentRole::User && record.kind == ContentKind::Text)
        .filter(|record| {
            let at = instant(&record.ts);
            let next = timeline.partition_point(|(ts, _)| *ts < at);
            timeline
                .get(next)
                .is_some_and(|(_, message_id)| selected.contains(message_id))
        })
        .filter_map(|record| record.text)
        .filter(|text| !text.is_empty())
        .collect()
}
