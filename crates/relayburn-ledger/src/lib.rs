//! Rust port of `@relayburn/ledger`.
//!
//! Mirrors the append-only JSONL ledger, content sidecar layout, lock
//! protocol, and SQLite archive surface. The TS package remains the source
//! of truth until the 2.0 cutover; this crate replicates its behavior so
//! both readers can interoperate against the same on-disk state.

pub mod archive;
pub mod errors;
pub mod file_adapter;
pub mod lock;
pub mod paths;
pub mod schema;
pub mod sidecar;

#[cfg(test)]
mod test_support;

pub use errors::{LedgerError, Result};
pub use lock::{
    with_lock, AcquireOptions, FAST_RETRIES, FAST_RETRY_DELAY_MS, SLOW_RETRIES,
    SLOW_RETRY_DELAY_MS, STALE_MS,
};
pub use paths::{
    archive_path, content_dir, content_file_path, cursors_path, hwm_path, is_valid_session_id,
    ledger_content_index_path, ledger_home, ledger_index_path, ledger_path, lock_path,
};
pub use schema::{
    compaction_id_hash, relationship_id_hash, stamp_matches, tool_result_event_id_hash,
    turn_content_fingerprint, turn_id_hash, user_turn_id_hash, CompactionLine, Enrichment,
    LedgerLine, LineKind, MessageIdRange, SessionRelationshipLine, StampLine, StampSelector,
    ToolResultEventLine, TurnLine, UserTurnLine,
};
