use super::*;

#[test]
fn report_merge_sums_components() {
    let mut a = IngestReport {
        scanned_sessions: 1,
        ingested_sessions: 2,
        appended_turns: 3,
        applied_pending_stamps: 4,
    };
    let b = IngestReport {
        scanned_sessions: 10,
        ingested_sessions: 20,
        appended_turns: 30,
        applied_pending_stamps: 40,
    };
    a.merge(&b);
    assert_eq!(a.scanned_sessions, 11);
    assert_eq!(a.ingested_sessions, 22);
    assert_eq!(a.appended_turns, 33);
    assert_eq!(a.applied_pending_stamps, 44);
}

#[test]
fn roots_default_to_home_layout() {
    let roots = IngestRoots::default();
    let claude = claude_projects_dir(&roots);
    let codex = codex_sessions_dir(&roots);
    let opencode = opencode_storage_dir(&roots);
    assert!(claude.ends_with(".claude/projects"));
    assert!(codex.ends_with(".codex/sessions"));
    assert!(opencode.ends_with(".local/share/opencode/storage"));
}

#[test]
fn roots_overrides_take_priority() {
    let roots = IngestRoots {
        claude_projects_dir: Some(PathBuf::from("/x/claude")),
        codex_sessions_dir: Some(PathBuf::from("/x/codex")),
        opencode_storage_dir: Some(PathBuf::from("/x/oc")),
        copilot_otel_files: Some(vec![]),
    };
    assert_eq!(claude_projects_dir(&roots), PathBuf::from("/x/claude"));
    assert_eq!(codex_sessions_dir(&roots), PathBuf::from("/x/codex"));
    assert_eq!(opencode_storage_dir(&roots), PathBuf::from("/x/oc"));
}

#[test]
fn source_fingerprint_is_stable_and_moves_on_change() {
    let tmp = tempfile::TempDir::new().unwrap();
    let roots = IngestRoots {
        claude_projects_dir: Some(tmp.path().join("claude")),
        codex_sessions_dir: Some(tmp.path().join("codex")),
        opencode_storage_dir: Some(tmp.path().join("opencode")),
        copilot_otel_files: Some(vec![]),
    };
    // Empty roots: well-formed, stable, and identical across calls.
    let empty = source_fingerprint(&roots);
    assert_eq!(empty, source_fingerprint(&roots));
    assert_eq!(empty, "0:0:0000000000000000");

    let project = roots.claude_projects_dir.as_ref().unwrap().join("proj");
    fs::create_dir_all(&project).unwrap();
    let file = project.join("s1.jsonl");
    fs::write(&file, "a\n").unwrap();
    let with_file = source_fingerprint(&roots);
    assert_ne!(with_file, empty, "adding a file must move the fingerprint");

    // Growing the file (size + mtime) must move it again.
    fs::write(&file, "a\nbb\n").unwrap();
    let appended = source_fingerprint(&roots);
    assert_ne!(
        appended, with_file,
        "appending bytes must move the fingerprint"
    );

    // Deleting the file (count drops) must move it back toward empty.
    fs::remove_file(&file).unwrap();
    let deleted = source_fingerprint(&roots);
    assert_ne!(
        deleted, appended,
        "deleting a file must move the fingerprint"
    );
    assert_eq!(deleted, empty, "back to zero files == empty fingerprint");
}

#[test]
fn source_fingerprint_moves_on_new_opencode_message_file() {
    // M1 regression: an OpenCode append lands as a NEW file under
    // message/<session>/, not as growth of the ses_*.json. Folding the
    // message-dir child count into the fingerprint guarantees the gate
    // re-opens even if the dir mtime granularity is too coarse to move.
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = tmp.path().join("opencode");
    let roots = IngestRoots {
        claude_projects_dir: Some(tmp.path().join("claude")),
        codex_sessions_dir: Some(tmp.path().join("codex")),
        opencode_storage_dir: Some(storage.clone()),
        copilot_otel_files: Some(vec![]),
    };

    let session_dir = storage.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    let session_file = session_dir.join("ses_abc.json");
    fs::write(&session_file, "{}").unwrap();

    let message_dir = storage.join("message").join("ses_abc");
    fs::create_dir_all(&message_dir).unwrap();
    fs::write(message_dir.join("msg_1.json"), "{}").unwrap();
    let before = source_fingerprint(&roots);

    // Add a second message file WITHOUT touching ses_abc.json. The session
    // file size/mtime are unchanged; only the message-dir child count moves.
    fs::write(message_dir.join("msg_2.json"), "{}").unwrap();
    let after = source_fingerprint(&roots);
    assert_ne!(
        after, before,
        "a new message file must move the fingerprint via the dir child count"
    );
}

mod persisted_codex_state {
    use serde_json::json;

    use super::*;
    use crate::reader::{UserTurnBlock, UserTurnBlockKind};

    #[test]
    fn last_completed_turn_reads_message_id_and_cache_read() {
        let turn =
            last_completed_turn_from_value(&json!({"messageId": "m1", "cacheRead": 42})).unwrap();
        assert_eq!(
            turn,
            CodexLastCompletedTurn {
                message_id: "m1".to_string(),
                cache_read: 42,
            }
        );
        assert_eq!(
            last_completed_turn_from_value(&last_completed_turn_to_value(&turn)),
            Some(turn)
        );
    }

    #[test]
    fn last_completed_turn_rejects_missing_or_mistyped_fields() {
        for v in [
            json!(null),
            json!(["m1", 42]),
            json!({"cacheRead": 42}),
            json!({"messageId": 7, "cacheRead": 42}),
            json!({"messageId": "m1"}),
            json!({"messageId": "m1", "cacheRead": -1}),
            json!({"messageId": "m1", "cacheRead": 1.5}),
            json!({"messageId": "m1", "cacheRead": "42"}),
        ] {
            assert_eq!(last_completed_turn_from_value(&v), None, "{v}");
        }
    }

    #[test]
    fn user_turn_slot_reads_blocks_preceding_id_and_ts() {
        let slot = user_turn_slot_from_value(&json!({
            "blocks": [
                {"kind": "tool_result", "toolUseId": "t1", "byteLen": 10, "approxTokens": 3, "isError": true},
                {"kind": "text", "byteLen": 4, "approxTokens": 1}
            ],
            "precedingMessageId": "m0",
            "ts": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            slot.blocks,
            vec![
                UserTurnBlock {
                    kind: UserTurnBlockKind::ToolResult,
                    tool_use_id: Some("t1".to_string()),
                    byte_len: 10,
                    approx_tokens: 3,
                    is_error: Some(true),
                },
                UserTurnBlock {
                    kind: UserTurnBlockKind::Text,
                    tool_use_id: None,
                    byte_len: 4,
                    approx_tokens: 1,
                    is_error: None,
                },
            ]
        );
        assert_eq!(slot.preceding_message_id.as_deref(), Some("m0"));
        assert_eq!(slot.ts, "2026-01-01T00:00:00Z");

        let round = user_turn_slot_from_value(&user_turn_slot_to_value(&slot)).unwrap();
        assert_eq!(round.blocks, slot.blocks);
        assert_eq!(round.preceding_message_id, slot.preceding_message_id);
        assert_eq!(round.ts, slot.ts);
    }

    #[test]
    fn user_turn_slot_treats_non_string_preceding_id_as_absent() {
        for v in [
            json!({"blocks": [], "ts": "t"}),
            json!({"blocks": [], "precedingMessageId": 5, "ts": "t"}),
        ] {
            let slot = user_turn_slot_from_value(&v).unwrap();
            assert!(slot.blocks.is_empty());
            assert_eq!(slot.preceding_message_id, None);
            assert_eq!(slot.ts, "t");
        }
    }

    #[test]
    fn user_turn_slot_rejects_missing_or_mistyped_fields() {
        for v in [
            json!(null),
            json!("slot"),
            json!({"ts": "t"}),
            json!({"blocks": "nope", "ts": "t"}),
            json!({"blocks": [{"kind": "text"}], "ts": "t"}),
            json!({"blocks": []}),
            json!({"blocks": [], "ts": 1}),
        ] {
            assert!(user_turn_slot_from_value(&v).is_none(), "{v}");
        }
    }
}
