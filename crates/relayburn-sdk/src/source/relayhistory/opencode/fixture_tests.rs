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

#[test]
fn a_turn_is_classified_by_the_prompt_that_asked_for_it() {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(&fixtures_root().join("opencode/simple"), dir.path());
    let parts = dir.path().join("storage/part/msg_simple_user");
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(
        parts.join("prt_user_text.json"),
        serde_json::json!({
            "id": "prt_user_text", "sessionID": "ses_simple", "messageID": "msg_simple_user",
            "type": "text", "text": "please review the parser"
        })
        .to_string(),
    )
    .unwrap();
    let records = fixture_records(
        Harness::Opencode,
        &dir.path().join("storage/session/global/ses_simple.json"),
    );
    assert_eq!(
        records.turns[0].activity,
        Some(crate::reader::ActivityCategory::Review)
    );
}

/// A prompt sent before a compaction did not prompt the summary turn the
/// compaction produced: the compaction is the user message that did.
#[test]
fn a_compaction_hides_the_prompts_before_it_from_the_summary_turn() {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(
        &fixtures_root().join("opencode/with-compaction"),
        dir.path(),
    );
    // `continue working` now lands between the first reply and the
    // compaction.
    let prompt = dir
        .path()
        .join("storage/message/ses_compact/msg_compact_u2.json");
    let text = std::fs::read_to_string(&prompt)
        .unwrap()
        .replace("1776999005000", "1776999002500");
    std::fs::write(&prompt, text).unwrap();
    let records = fixture_records(
        Harness::Opencode,
        &dir.path().join("storage/session/global/ses_compact.json"),
    );
    let prompts: Vec<(&str, Option<&str>)> = records
        .user_turns
        .iter()
        .map(|u| (u.user_uuid.as_str(), u.following_message_id.as_deref()))
        .collect();
    assert_eq!(prompts, [("msg_compact_u1", Some("msg_compact_a1"))]);
}

/// A prompt written in the same millisecond as an assistant turn was not
/// sent after it, so it does not bridge to the next turn.
#[test]
fn a_prompt_in_the_previous_turns_millisecond_does_not_bridge() {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(
        &fixtures_root().join("opencode/user-turn-blocks"),
        dir.path(),
    );
    let prompt = dir.path().join("storage/message/ses_utb/msg_utb_u2.json");
    let text = std::fs::read_to_string(&prompt)
        .unwrap()
        .replace("1776990002000", "1776990001000");
    std::fs::write(&prompt, text).unwrap();
    let records = fixture_records(
        Harness::Opencode,
        &dir.path().join("storage/session/global/ses_utb.json"),
    );
    let bridge = records
        .user_turns
        .iter()
        .find(|u| u.following_message_id.as_deref() == Some("msg_utb_a2"))
        .expect("the bridge into the second turn");
    assert_eq!(bridge.user_uuid, "ses_utb:msg_utb_a1->msg_utb_a2");
    assert!(bridge
        .blocks
        .iter()
        .all(|b| b.kind == crate::reader::UserTurnBlockKind::ToolResult));
}
