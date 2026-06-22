//! Conformance tests for the tool_output_bloat module — extracted verbatim from
//! the former inline `#[cfg(test)] mod tests` block (included via `#[path]`).

    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{ToolCall, ToolResultEventSource, ToolResultStatus, Usage, UserTurnBlock};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn loaded(path: &str, env: serde_json::Value) -> LoadedClaudeSettings {
        let settings: ClaudeSettings = serde_json::from_value(json!({ "env": env })).unwrap();
        LoadedClaudeSettings {
            path: PathBuf::from(path),
            settings,
        }
    }

    fn loaded_no_env(path: &str) -> LoadedClaudeSettings {
        LoadedClaudeSettings {
            path: PathBuf::from(path),
            settings: ClaudeSettings::default(),
        }
    }

    fn evt(
        session_id: &str,
        tool_use_id: &str,
        event_index: u64,
        message_id: Option<&str>,
    ) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.to_string(),
            message_id: message_id.map(String::from),
            tool_use_id: tool_use_id.to_string(),
            call_index: None,
            event_index,
            ts: None,
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: None,
            output_bytes: None,
            output_truncated: None,
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn evt_with(
        source: SourceKind,
        session_id: &str,
        tool_use_id: &str,
        event_index: u64,
        message_id: Option<&str>,
        event_source: ToolResultEventSource,
        content_length: Option<u64>,
        call_index: Option<u64>,
    ) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            message_id: message_id.map(String::from),
            tool_use_id: tool_use_id.to_string(),
            call_index,
            event_index,
            ts: None,
            status: ToolResultStatus::Completed,
            event_source,
            content_length,
            output_bytes: content_length,
            output_truncated: None,
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn user_turn_with(
        source: SourceKind,
        session_id: &str,
        user_uuid: &str,
        preceding: &str,
        following: &str,
        tool_use_id: &str,
        byte_len: u64,
        approx_tokens: u64,
    ) -> UserTurnRecord {
        UserTurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            user_uuid: user_uuid.to_string(),
            ts: "2026-04-20T00:00:00.500Z".to_string(),
            preceding_message_id: Some(preceding.to_string()),
            following_message_id: Some(following.to_string()),
            blocks: vec![UserTurnBlock {
                kind: UserTurnBlockKind::ToolResult,
                tool_use_id: Some(tool_use_id.to_string()),
                byte_len,
                approx_tokens,
                is_error: None,
            }],
        }
    }

    fn turn_with(
        source: SourceKind,
        session_id: &str,
        message_id: &str,
        turn_index: u64,
        tool_calls: Vec<ToolCall>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            session_path: None,
            message_id: message_id.to_string(),
            turn_index,
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 5,
                reasoning: 0,
                cache_read: 100,
                cache_create_5m: 50,
                cache_create_1h: 0,
            },
            tool_calls,
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn tc(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            target: None,
            args_hash: "hash".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    // -------------------------------------------------------------------
    // Signal A — static-config check
    // -------------------------------------------------------------------

    #[test]
    fn signal_a_flags_oversized_bash_max_output_length() {
        let settings = vec![loaded(
            "/home/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        });
        assert_eq!(out.len(), 1);
        let f = &out[0];
        assert_eq!(f.kind, ToolOutputBloatKind::StaticConfig);
        assert_eq!(f.source, SourceKind::ClaudeCode);
        assert_eq!(f.tool_name, "Bash");
        assert_eq!(f.configured_limit, Some(80_000));
        assert_eq!(f.evidenced_max_output, 20_000);
        assert_eq!(f.occurrence_count, 1);
        assert_eq!(f.cost, 0.0);
        assert_eq!(
            f.evidence,
            vec!["/home/u/.claude/settings.json".to_string()]
        );
    }

    #[test]
    fn signal_a_does_not_flag_at_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "60000" }),
        )];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_unit_conversion_under_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "50000" }),
        )];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_no_env_block() {
        let settings = vec![loaded_no_env("/u/.claude/settings.json")];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_project_overrides_user() {
        let settings = vec![
            loaded(
                "/u/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
            ),
            loaded(
                "/cwd/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "60000" }),
            ),
        ];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_project_path_reported_when_project_overrides_to_oversized() {
        let settings = vec![
            loaded(
                "/u/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "15000" }),
            ),
            loaded(
                "/cwd/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "99999" }),
            ),
        ];
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].evidence,
            vec!["/cwd/.claude/settings.json".to_string()]
        );
        assert_eq!(out[0].configured_limit, Some(99_999));
    }

    #[test]
    fn signal_a_honors_custom_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "5000" }),
        )];
        let tight = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: Some(1_000),
            settings: settings.clone(),
        });
        assert_eq!(tight.len(), 1);
        let loose = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: Some(10_000),
            settings,
        });
        assert!(loose.is_empty());
    }

    // -------------------------------------------------------------------
    // Filesystem loader
    // -------------------------------------------------------------------

    #[test]
    fn load_settings_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        assert!(load_claude_settings(dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn load_settings_returns_none_for_malformed_json() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("bad.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_claude_settings(&p).is_none());
    }

    #[test]
    fn load_settings_reads_env_block() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            json!({ "env": { BASH_MAX_OUTPUT_ENV_KEY: "80000" } }).to_string(),
        )
        .unwrap();
        let loaded = load_claude_settings(&p).expect("loads");
        assert_eq!(loaded.path, p);
        let env = loaded.settings.env.as_ref().expect("env present");
        assert_eq!(
            env.get(BASH_MAX_OUTPUT_ENV_KEY).and_then(|v| v.as_str()),
            Some("80000"),
        );
    }

    #[test]
    fn load_and_detect_end_to_end() {
        let dir = tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let p = claude_dir.join("settings.json");
        std::fs::write(
            &p,
            json!({ "env": { BASH_MAX_OUTPUT_ENV_KEY: "80000" } }).to_string(),
        )
        .unwrap();
        let loaded = load_claude_settings(&p).expect("loads");
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings: vec![loaded],
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].configured_limit, Some(80_000));
    }

    // -------------------------------------------------------------------
    // Signal B — observed bloat across sessions
    // -------------------------------------------------------------------

    #[test]
    fn signal_b_flags_bash_above_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.kind, ToolOutputBloatKind::ObservedBloat);
        assert_eq!(b.source, SourceKind::ClaudeCode);
        assert_eq!(b.tool_name, "Bash");
        assert_eq!(b.occurrence_count, 1);
        assert_eq!(b.evidenced_max_output, 20_000);
        assert_eq!(b.evidence, vec!["s1".to_string()]);
        assert!(b.cost > 0.0, "cost should be priced via the model rate");
    }

    #[test]
    fn signal_b_does_not_flag_below_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            40_000,
            10_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn signal_b_aggregates_into_single_bucket() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt("s1", "tu_a", 0, Some("m1")),
            evt("s2", "tu_b", 0, Some("m2")),
            evt("s3", "tu_c", 0, Some("m3")),
        ];
        let user_turns = vec![
            user_turn_with(
                SourceKind::ClaudeCode,
                "s1",
                "u1",
                "m1",
                "m2",
                "tu_a",
                80_000,
                20_000,
            ),
            user_turn_with(
                SourceKind::ClaudeCode,
                "s2",
                "u2",
                "m2",
                "m3",
                "tu_b",
                100_000,
                25_000,
            ),
            user_turn_with(
                SourceKind::ClaudeCode,
                "s3",
                "u3",
                "m3",
                "m4",
                "tu_c",
                120_000,
                30_000,
            ),
        ];
        let turns = vec![
            turn_with(
                SourceKind::ClaudeCode,
                "s1",
                "m1",
                0,
                vec![tc("tu_a", "Bash")],
            ),
            turn_with(
                SourceKind::ClaudeCode,
                "s2",
                "m2",
                0,
                vec![tc("tu_b", "Bash")],
            ),
            turn_with(
                SourceKind::ClaudeCode,
                "s3",
                "m3",
                0,
                vec![tc("tu_c", "Bash")],
            ),
        ];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.occurrence_count, 3);
        assert_eq!(b.evidenced_max_output, 30_000);
        assert_eq!(b.evidence.len(), 3);
    }

    #[test]
    fn signal_b_emits_one_bucket_per_source_tool_pair() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                0,
                Some("m1"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
            evt_with(
                SourceKind::Codex,
                "s2",
                "call_b",
                0,
                Some("m2"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
            evt_with(
                SourceKind::Opencode,
                "s3",
                "opc_c",
                0,
                Some("m3"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
        ];
        let user_turns = vec![
            user_turn_with(
                SourceKind::ClaudeCode,
                "s1",
                "u1",
                "m1",
                "m2",
                "tu_a",
                80_000,
                20_000,
            ),
            user_turn_with(
                SourceKind::Codex,
                "s2",
                "u2",
                "m2",
                "m3",
                "call_b",
                90_000,
                22_500,
            ),
            user_turn_with(
                SourceKind::Opencode,
                "s3",
                "u3",
                "m3",
                "m4",
                "opc_c",
                85_000,
                21_250,
            ),
        ];
        let turns = vec![
            turn_with(
                SourceKind::ClaudeCode,
                "s1",
                "m1",
                0,
                vec![tc("tu_a", "Bash")],
            ),
            turn_with(
                SourceKind::Codex,
                "s2",
                "m2",
                0,
                vec![tc("call_b", "shell")],
            ),
            turn_with(
                SourceKind::Opencode,
                "s3",
                "m3",
                0,
                vec![tc("opc_c", "bash")],
            ),
        ];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 3);
        let mut sources: Vec<SourceKind> = out.iter().map(|b| b.source).collect();
        sources.sort_by_key(|s| match s {
            SourceKind::ClaudeCode => 0,
            SourceKind::Codex => 1,
            SourceKind::Opencode => 2,
            _ => 3,
        });
        assert_eq!(
            sources,
            vec![
                SourceKind::ClaudeCode,
                SourceKind::Codex,
                SourceKind::Opencode
            ]
        );
        for b in &out {
            assert_eq!(b.tool_name, "Bash");
        }
    }

    #[test]
    fn signal_b_skips_events_without_user_turn_blocks() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &[],
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn signal_b_honors_custom_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            4_000,
            1_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let def = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(def.is_empty());
        let tight = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: Some(500),
            min_occurrences: None,
        });
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].evidenced_max_output, 1_000);
    }

    #[test]
    fn signal_b_falls_back_to_unknown_tool_name() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "orphan", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "orphan",
            80_000,
            20_000,
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &[],
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool_name, "<unknown>");
        assert_eq!(out[0].cost, 0.0);
    }

    #[test]
    fn signal_b_does_not_double_count_carrier_plus_subagent_notification() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                0,
                Some("m1"),
                ToolResultEventSource::ToolResult,
                None,
                Some(0),
            ),
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                1,
                Some("m1"),
                ToolResultEventSource::SubagentNotification,
                Some(200),
                Some(1),
            ),
        ];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].occurrence_count, 1);
        assert_eq!(out[0].evidenced_max_output, 20_000);
    }

    // -------------------------------------------------------------------
    // Top-level orchestration
    // -------------------------------------------------------------------

    #[test]
    fn orchestration_runs_both_signals() {
        let pricing = load_builtin_pricing();
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 2);
        let mut kinds: Vec<ToolOutputBloatKind> = out.iter().map(|b| b.kind).collect();
        kinds.sort_by_key(|k| match k {
            ToolOutputBloatKind::ObservedBloat => 0,
            ToolOutputBloatKind::StaticConfig => 1,
        });
        assert_eq!(
            kinds,
            vec![
                ToolOutputBloatKind::ObservedBloat,
                ToolOutputBloatKind::StaticConfig,
            ]
        );
    }

    #[test]
    fn orchestration_signal_a_only() {
        let pricing = load_builtin_pricing();
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &[],
            user_turns: &[],
            turns: &[],
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ToolOutputBloatKind::StaticConfig);
    }

    #[test]
    fn orchestration_signal_b_only() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &[],
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ToolOutputBloatKind::ObservedBloat);
    }

    // -------------------------------------------------------------------
    // WasteFinding adapter
    // -------------------------------------------------------------------

    #[test]
    fn finding_adapter_signal_a_paste_targets_settings_json() {
        let f = tool_output_bloat_to_finding(&ToolOutputBloat {
            source: SourceKind::ClaudeCode,
            kind: ToolOutputBloatKind::StaticConfig,
            tool_name: "Bash".to_string(),
            configured_limit: Some(80_000),
            evidenced_max_output: 20_000,
            evidenced_p95_output: None,
            occurrence_count: 1,
            cost: 0.0,
            evidence: vec!["/u/.claude/settings.json".to_string()],
        });
        assert_eq!(f.kind, "tool-output-bloat");
        assert_eq!(f.actions.len(), 1);
        match &f.actions[0] {
            WasteAction::Paste { label, text } => {
                assert!(label.contains("settings.json"), "label: {label}");
                assert!(text.contains(BASH_MAX_OUTPUT_ENV_KEY), "text: {text}");
                assert!(
                    text.contains("\"60000\""),
                    "text should target 60000 chars: {text}"
                );
            }
            other => panic!("expected Paste action, got {other:?}"),
        }
        assert_eq!(f.estimated_savings.tokens_per_session, Some(20_000));
    }

    #[test]
    fn finding_adapter_signal_b_emits_instruction_paste() {
        let f = tool_output_bloat_to_finding(&ToolOutputBloat {
            source: SourceKind::Codex,
            kind: ToolOutputBloatKind::ObservedBloat,
            tool_name: "shell".to_string(),
            configured_limit: None,
            evidenced_max_output: 25_000,
            evidenced_p95_output: Some(24_000),
            occurrence_count: 4,
            cost: 0.07,
            evidence: vec!["s1".to_string(), "s2".to_string()],
        });
        assert_eq!(f.kind, "tool-output-bloat");
        assert_eq!(f.severity, WasteSeverity::Warn);
        assert!(f.title.contains("codex shell"), "title: {}", f.title);
        assert!(f.title.contains("4×"), "title: {}", f.title);
        assert!(f.detail.contains("head"), "detail: {}", f.detail);
        assert!(f.detail.contains("tail"), "detail: {}", f.detail);
        assert!(f.detail.contains("grep"), "detail: {}", f.detail);
        assert!(matches!(f.actions[0], WasteAction::Paste { .. }));
    }

    // -------------------------------------------------------------------
    // Fixture-driven integration coverage
    // -------------------------------------------------------------------

    fn workspace_fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join(rel)
    }

    #[test]
    fn fixture_settings_json_oversized_bash_output_length() {
        let path = workspace_fixture("claude/settings/oversized-bash-output-length.json");
        let loaded = load_claude_settings(&path).expect("fixture loads");
        let result = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings: vec![loaded],
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].configured_limit, Some(80_000));
        assert_eq!(
            result[0].evidence,
            vec![path.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn fixture_claude_oversized_bash_output_enriched_path() {
        use crate::reader::{parse_claude_session, ClaudeParseOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("claude/oversized-bash-output.jsonl");
        let parsed = parse_claude_session(&path, &ClaudeParseOptions::default()).expect("parses");
        // cl100k tokenizes repeated single-char content far below the
        // bytes/4 heuristic; we don't have cl100k wired here so the
        // detector falls back to bytes/4 either way. Use a low threshold
        // so the assertion still trips on the synthetic content.
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &parsed.user_turns,
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: Some(5_000),
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::ClaudeCode);
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output > 5_000);
    }

    #[test]
    fn fixture_claude_oversized_bash_output_content_length_fallback() {
        use crate::reader::{parse_claude_session, ClaudeParseOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("claude/oversized-bash-output.jsonl");
        let parsed = parse_claude_session(&path, &ClaudeParseOptions::default()).expect("parses");
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &[],
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::ClaudeCode);
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
    }

    #[test]
    fn fixture_codex_oversized_shell_output() {
        use crate::reader::codex::{parse_codex_session, ParseCodexOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("codex/oversized-shell-output.jsonl");
        let parsed = parse_codex_session(&path, &ParseCodexOptions::default()).expect("parses");
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &parsed.user_turns,
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::Codex);
        // Codex `shell` normalizes to canonical `Bash`.
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
    }

    #[test]
    fn fixture_opencode_synthesized_bash() {
        let pricing = load_builtin_pricing();
        let events = vec![evt_with(
            SourceKind::Opencode,
            "ses_bloat",
            "opc_bash_1",
            0,
            Some("msg_bloat"),
            ToolResultEventSource::ToolResult,
            None,
            None,
        )];
        let user_turns = vec![user_turn_with(
            SourceKind::Opencode,
            "ses_bloat",
            "u_bloat",
            "msg_bloat",
            "msg_bloat_next",
            "opc_bash_1",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::Opencode,
            "ses_bloat",
            "msg_bloat",
            0,
            vec![tc("opc_bash_1", "bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::Opencode);
        assert_eq!(out[0].tool_name, "Bash");
    }
