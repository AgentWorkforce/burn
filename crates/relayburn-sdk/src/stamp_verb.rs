//! `write_stamp` — direct stamp write by exact session id or message id.
//!
//! Companion to `write_pending_stamp` (sidecar manifest, matched at ingest
//! time by cwd + spawnerPid + spawnStartTs). When a launcher knows the
//! session id up front — e.g. it preallocated a Claude `--session-id` UUID
//! before spawn — calling [`write_stamp`] folds the enrichment straight
//! onto the ledger by selector, skipping the manifest dance entirely. This
//! is the more reliable path: no path-matching race, no orphan manifest if
//! the spawn fails.
//!
//! The verb is exposed as a free function and as a [`LedgerHandle`] method,
//! mirroring the rest of the SDK surface.
//!
//! Empty selectors (neither `session_id` nor `message_id` set) are
//! rejected via [`crate::StampError::EmptySelector`] — a stamp with no
//! selector would label every turn, which is never what the caller wants.

use std::path::PathBuf;

use anyhow::Result;

use crate::{Enrichment, Ledger, LedgerHandle, LedgerOpenOptions, Stamp, StampSelector};

/// Options for [`write_stamp`]. At least one of `session_id` or
/// `message_id` must be set.
#[derive(Debug, Clone, Default)]
pub struct WriteStampOptions {
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub enrichment: Enrichment,
    /// ISO-8601 timestamp the caller observed. Defaults to "now" formatted
    /// `YYYY-MM-DDTHH:MM:SSZ` when omitted.
    pub ts: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Append a stamp targeting the given session / message selector.
    pub fn write_stamp(&mut self, opts: WriteStampOptions) -> Result<()> {
        let stamp = build_stamp(&opts)?;
        self.inner.append_stamp(&stamp)?;
        Ok(())
    }
}

/// Open the ledger, write the stamp, drop the handle.
pub fn write_stamp(opts: WriteStampOptions) -> Result<()> {
    let stamp = build_stamp(&opts)?;
    let lo = match opts.ledger_home.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    let mut handle = Ledger::open(lo)?;
    handle.inner.append_stamp(&stamp)?;
    Ok(())
}

fn build_stamp(opts: &WriteStampOptions) -> Result<Stamp> {
    let selector = StampSelector {
        session_id: opts.session_id.clone(),
        message_id: opts.message_id.clone(),
        range: None,
    };
    let ts = opts
        .ts
        .clone()
        .unwrap_or_else(|| now_iso(&std::time::SystemTime::now()));
    Stamp::new(ts, selector, opts.enrichment.clone()).map_err(Into::into)
}

fn now_iso(now: &std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
    );
    dt.format(&fmt).expect("format z iso")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn empty_selector_is_rejected() {
        let err = write_stamp(WriteStampOptions {
            session_id: None,
            message_id: None,
            enrichment: BTreeMap::new(),
            ts: None,
            ledger_home: Some(std::env::temp_dir()),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("selector"),
            "expected empty-selector error, got: {err}"
        );
    }

    #[test]
    fn stamp_round_trips_session_selector() {
        let dir = tempfile::tempdir().unwrap();
        let mut enrichment = Enrichment::new();
        enrichment.insert("spawner".into(), "pear".into());
        enrichment.insert("on_relay".into(), "true".into());
        write_stamp(WriteStampOptions {
            session_id: Some("abc-123".into()),
            message_id: None,
            enrichment: enrichment.clone(),
            ts: Some("2026-05-21T12:00:00Z".into()),
            ledger_home: Some(dir.path().to_path_buf()),
        })
        .unwrap();

        let opts = LedgerOpenOptions::with_home(dir.path());
        let handle = Ledger::open(opts).unwrap();
        let stamps = handle.inner.list_stamps().unwrap();
        assert_eq!(stamps.len(), 1, "expected exactly one stamp");
        assert_eq!(stamps[0].selector.session_id.as_deref(), Some("abc-123"));
        assert_eq!(stamps[0].enrichment, enrichment);
    }

    #[test]
    fn default_ts_is_iso_z() {
        let dir = tempfile::tempdir().unwrap();
        let mut enrichment = Enrichment::new();
        enrichment.insert("k".into(), "v".into());
        write_stamp(WriteStampOptions {
            session_id: Some("s".into()),
            message_id: None,
            enrichment,
            ts: None,
            ledger_home: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        let handle =
            Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
        let stamps = handle.inner.list_stamps().unwrap();
        let ts = &stamps[0].ts;
        assert!(
            ts.ends_with('Z') && ts.contains('T') && ts.len() == 20,
            "ts should be YYYY-MM-DDTHH:MM:SSZ, got {ts}"
        );
    }
}
