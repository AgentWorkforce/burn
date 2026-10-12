//! The session fixture corpus and the frozen record snapshots the parity
//! suite holds burn's relayhistory mapping to.

use std::path::{Path, PathBuf};

use super::locate::{load_session, SessionLocator};
use super::{records_with_children, Billing, SessionRecords};
use crate::reader::Harness;

/// The records burn derives from one session artifact, read through
/// relayhistory the way `burn analyze --input` reads it.
pub(crate) fn fixture_records(harness: Harness, path: &Path) -> SessionRecords {
    let locator = SessionLocator::Path {
        harness,
        path: path.to_path_buf(),
    };
    let loaded = load_session(&locator, &Default::default())
        .unwrap_or_else(|e| panic!("load {}: {e:#}", path.display()));
    records_with_children(&loaded.evidence, &loaded.children, &Billing::settled())
}

pub(crate) fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

pub(super) fn snapshot_dir() -> PathBuf {
    fixtures_root().join("sourcing-snapshots")
}

/// `records` as JSON with each `(path, placeholder)` replacement applied in
/// order (both the path and its canonical form), and `sessionPath` dropped:
/// where a session file lives is not something burn derives.
pub(super) fn render_value(
    records: &SessionRecords,
    replacements: &[(&Path, &str)],
) -> serde_json::Value {
    let mut json = serde_json::to_string(records).unwrap();
    for (path, placeholder) in replacements {
        let canonical = path.canonicalize().unwrap();
        for form in [canonical.as_path(), path] {
            json = json.replace(&*form.to_string_lossy(), placeholder);
        }
    }
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    if let Some(turns) = value.get_mut("turns").and_then(|t| t.as_array_mut()) {
        for turn in turns {
            if let Some(obj) = turn.as_object_mut() {
                obj.remove("sessionPath");
            }
        }
    }
    value
}
