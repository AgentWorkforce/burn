//! What an OpenCode compaction says the context held before it.

use std::path::Path;

use crate::reader::Harness;
use crate::source::fixtures::{fixture_records, fixtures_root};

fn copy_tree(src: &Path, dst: &Path) {
    if src.is_file() {
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(src, dst).unwrap();
        return;
    }
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        copy_tree(&path, &dst.join(path.file_name().unwrap()));
    }
}

#[test]
fn an_assistant_turn_written_with_the_compaction_did_not_precede_it() {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(
        &fixtures_root().join("opencode/with-compaction"),
        dir.path(),
    );
    // The summary turn starts in the same millisecond as the compaction.
    let summary = dir
        .path()
        .join("storage/message/ses_compact/msg_compact_summary.json");
    let text = std::fs::read_to_string(&summary)
        .unwrap()
        .replace("1776999004000", "1776999003000");
    std::fs::write(&summary, text).unwrap();
    let records = fixture_records(
        Harness::Opencode,
        &dir.path().join("storage/session/global/ses_compact.json"),
    );
    let preceding: Vec<Option<&str>> = records
        .compactions
        .iter()
        .map(|c| c.preceding_message_id.as_deref())
        .collect();
    assert_eq!(preceding, [Some("msg_compact_a1")]);
}
