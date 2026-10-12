//! OpenCode fixture sessions edited to pin one mapping rule each.

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

#[test]
fn a_text_only_turn_is_classified_by_its_own_words() {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(&fixtures_root().join("opencode/simple"), dir.path());
    let part = dir
        .path()
        .join("storage/part/msg_simple_asst/prt_simple_2.json");
    let text = std::fs::read_to_string(&part)
        .unwrap()
        .replace("Hello.", "Here is the roadmap.");
    std::fs::write(&part, text).unwrap();
    let records = fixture_records(
        Harness::Opencode,
        &dir.path().join("storage/session/global/ses_simple.json"),
    );
    assert_eq!(
        records.turns[0].activity,
        Some(crate::reader::ActivityCategory::Planning)
    );
}
