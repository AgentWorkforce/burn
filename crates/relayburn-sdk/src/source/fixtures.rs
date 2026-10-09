//! The session fixture corpus and the frozen record snapshots the parity
//! suite holds burn's relayhistory mapping to.

use std::path::{Path, PathBuf};

use super::locate::{load_session, SessionLocator};
use super::{records_with_children, SessionRecords};
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
    records_with_children(&loaded.evidence, &loaded.children)
}

pub(crate) fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

pub(super) fn snapshot_dir() -> PathBuf {
    fixtures_root().join("sourcing-snapshots")
}

/// `records` as JSON with every occurrence of `root` (and its canonical
/// form) replaced by `<fixtures>`, and `sessionPath` dropped: where a
/// session file lives is not something burn derives.
pub(super) fn render_value(records: &SessionRecords, root: &Path) -> serde_json::Value {
    let mut json = serde_json::to_string(records).unwrap();
    let canonical = root.canonicalize().unwrap();
    for prefix in [canonical.as_path(), root] {
        json = json.replace(&*prefix.to_string_lossy(), "<fixtures>");
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
