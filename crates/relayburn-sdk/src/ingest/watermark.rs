//! The ledger's position in the relayhistory change feed.
//!
//! The watermark lives in the ledger (`archive_state`'s ingest cursors), not
//! as a named cursor in the store: it describes what this ledger holds, so a
//! reset ledger, a second `RELAYBURN_HOME` or a copied one each resume from
//! their own position. A ledger with no watermark, or one the store never
//! issued (its database was replaced), resyncs every session the store
//! holds; appends are idempotent, so that is a reconciliation, never a
//! duplicate.

use std::collections::BTreeSet;

use ai_hist::{ChangeKind, ChangeQuery, IdentityQuery, SessionIdentity, SessionStore, Watermark};
use serde_json::{json, Value};

use crate::ledger::Ledger;

const KEY: &str = "relayhistory";

/// Change kinds that move a session's records: its catalog row and its
/// evidence. Prompt history, presences and connector observations do not.
const KINDS: [ChangeKind; 6] = [
    ChangeKind::Session,
    ChangeKind::SessionEvent,
    ChangeKind::ToolCall,
    ChangeKind::FileEdit,
    ChangeKind::SessionMarker,
    ChangeKind::Relationship,
];

pub(super) fn stored(ledger: &Ledger) -> Option<Watermark> {
    let cursors: Value = serde_json::from_str(&ledger.read_cursors().ok()?).ok()?;
    serde_json::from_value(cursors.get(KEY)?.clone()).ok()
}

pub(super) fn store(ledger: &mut Ledger, watermark: Watermark) -> anyhow::Result<()> {
    ledger.write_cursors(&json!({ KEY: watermark }).to_string())?;
    Ok(())
}

/// The sessions the store changed after `from`, and the head they were read
/// up to. Without a usable `from`, every session the store holds.
pub(super) fn changed_since(
    store: &SessionStore,
    from: Option<Watermark>,
) -> anyhow::Result<(BTreeSet<SessionIdentity>, Watermark)> {
    if let Some(from) = from {
        let query = ChangeQuery::default()
            .kinds(KINDS)
            .batch(ai_hist::MAX_CHANGE_BATCH);
        match store.changes_since(from, query) {
            Ok(changes) => return drain(changes),
            Err(ai_hist::Error::WatermarkAheadOfStore(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    // The head is read before the walk: a session written during it is
    // read again next time rather than missed.
    let head = store.head_revision()?;
    Ok((every_session(store)?, head))
}

fn drain(mut changes: ai_hist::Changes) -> anyhow::Result<(BTreeSet<SessionIdentity>, Watermark)> {
    let mut sessions = BTreeSet::new();
    for change in changes.by_ref() {
        let change = change?;
        if change.source.is_some() && !change.session_id.is_empty() {
            sessions.insert(SessionIdentity::new(change.source_name, change.session_id));
        }
    }
    Ok((sessions, changes.head()))
}

fn every_session(store: &SessionStore) -> anyhow::Result<BTreeSet<SessionIdentity>> {
    let mut sessions = BTreeSet::new();
    let mut after: Option<SessionIdentity> = None;
    loop {
        let mut query = IdentityQuery::default().limit(PAGE);
        if let Some(last) = after.take() {
            query = query.after(last);
        }
        let page = store.session_identities(query)?;
        let Some(last) = page.last().cloned() else {
            return Ok(sessions);
        };
        sessions.extend(page);
        after = Some(last);
    }
}

const PAGE: usize = 10_000;
