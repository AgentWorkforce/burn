//! Conformance tests for the hotspots module — extracted verbatim from the
//! former inline `#[cfg(test)] mod tests` block (included via `#[path]`).

    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{
        parse_bash_command, ContentRole, ContentToolResult, SourceKind, ToolCall, Usage,
        UserTurnBlock,
    };
    use serde_json::json;

    fn empty_usage() -> Usage {
        Usage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn turn(
        session_id: &str,
        message_id: &str,
        turn_index: u64,
        ts: &str,
        model: &str,
        usage: Usage,
        tool_calls: Vec<ToolCall>,
        source: SourceKind,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index,
            ts: ts.into(),
            model: model.into(),
            project: None,
            project_key: None,
            usage,
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

    fn tc(id: &str, name: &str, target: Option<&str>) -> ToolCall {
        let target_part = target.unwrap_or(id);
        ToolCall {
            id: id.into(),
            name: name.into(),
            target: target.map(String::from),
            args_hash: format!("{name}:{target_part}"),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tc_with_hash(id: &str, name: &str, target: &str, args_hash: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            target: Some(target.into()),
            args_hash: args_hash.into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tool_result_content(
        session_id: &str,
        tool_use_id: &str,
        text: &str,
        ts: &str,
    ) -> ContentRecord {
        ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            message_id: format!("m-{tool_use_id}"),
            ts: ts.into(),
            role: ContentRole::ToolResult,
            kind: ContentKind::ToolResult,
            text: None,
            tool_use: None,
            tool_result: Some(ContentToolResult {
                tool_use_id: tool_use_id.into(),
                content: json!(text),
                is_error: None,
            }),
        }
    }

    fn user_turn(session_id: &str, user_uuid: &str, blocks: Vec<UserTurnBlock>) -> UserTurnRecord {
        UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            user_uuid: user_uuid.into(),
            ts: "2026-04-20T00:00:00.500Z".into(),
            preceding_message_id: Some("msg-0".into()),
            following_message_id: Some("msg-1".into()),
            blocks,
        }
    }

    fn tool_result_block(tool_use_id: &str, byte_len: u64, approx_tokens: u64) -> UserTurnBlock {
        UserTurnBlock {
            kind: UserTurnBlockKind::ToolResult,
            tool_use_id: Some(tool_use_id.into()),
            byte_len,
            approx_tokens,
            is_error: None,
        }
    }

    fn bash_attribution(
        command: &str,
        args_hash: &str,
        total_cost: f64,
        initial_tokens: f64,
        persistence_tokens: f64,
        riding_turns: u64,
    ) -> ToolAttribution {
        ToolAttribution {
            tool_use_id: format!("tu-{args_hash}"),
            tool_name: "Bash".into(),
            target: Some(command.into()),
            args_hash: args_hash.into(),
            session_id: "s-bash-verb".into(),
            emit_turn_index: 0,
            emit_ts: "2026-04-20T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            subagent_type: None,
            result_tokens: 0,
            result_bytes_estimated: true,
            output_bytes: None,
            output_truncated: None,
            initial_cost: total_cost,
            initial_tokens,
            persistence_cost: 0.0,
            persistence_tokens,
            riding_turns,
            total_cost,
        }
    }

    #[test]
    fn attributes_persistence_of_8k_read_across_20_ride_along_turns_within_10_pct() {
        let pricing = load_builtin_pricing();
        let rate = pricing
            .get("claude-sonnet-4-6")
            .expect("sonnet present")
            .clone();
        const READ_TOKENS: u64 = 8000;
        let read_text: String = "x".repeat((READ_TOKENS as usize) * 4);

        let session_id = "s-hotspots-1";
        let mut turns: Vec<TurnRecord> = Vec::new();

        // Turn 0: assistant emits the Read tool_use.
        turns.push(turn(
            session_id,
            "msg-0",
            0,
            "2026-04-20T00:00:00.000Z",
            "claude-sonnet-4-6",
            Usage {
                input: 200,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![tc("tu_read_1", "Read", Some("/src/big.ts"))],
            SourceKind::ClaudeCode,
        ));

        // Turn 1 pays initial: 8000 tokens enter as fresh input.
        turns.push(turn(
            session_id,
            "msg-1",
            1,
            "2026-04-20T00:00:01.000Z",
            "claude-sonnet-4-6",
            Usage {
                input: READ_TOKENS,
                output: 30,
                reasoning: 0,
                cache_read: 250,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![],
            SourceKind::ClaudeCode,
        ));

        // Turns 2..=21: 20 ride-along turns each with cacheRead >= READ_TOKENS.
        for i in 2..=21u64 {
            turns.push(turn(
                session_id,
                &format!("msg-{i}"),
                i,
                &format!("2026-04-20T00:00:{:02}.000Z", i),
                "claude-sonnet-4-6",
                Usage {
                    input: 50,
                    output: 30,
                    reasoning: 0,
                    cache_read: READ_TOKENS + 2000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ));
        }

        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_read_1",
                &read_text,
                "2026-04-20T00:00:00.500Z",
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        assert_eq!(result.attributions.len(), 1);
        let a = &result.attributions[0];
        assert_eq!(a.tool_use_id, "tu_read_1");

        let expected_initial = (READ_TOKENS as f64 / 1_000_000.0) * rate.input;
        let expected_persistence = 20.0 * (READ_TOKENS as f64 / 1_000_000.0) * rate.cache_read;
        let expected_total = expected_initial + expected_persistence;
        assert!(
            (a.total_cost - expected_total).abs() <= expected_total * 0.10,
            "total={} expected={} diff>10%",
            a.total_cost,
            expected_total
        );
        assert_eq!(a.riding_turns, 20);
    }

    #[test]
    fn aggregates_by_file_and_ranks_most_expensive_read_first() {
        let pricing = load_builtin_pricing();
        let session_id = "s-files";
        const READ_TOKENS: u64 = 5000;
        const SMALL_TOKENS: u64 = 200;
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_a", "Read", Some("/big.ts")),
                    tc("tu_b", "Read", Some("/small.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: READ_TOKENS + SMALL_TOKENS,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: READ_TOKENS + SMALL_TOKENS + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-3",
                3,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: READ_TOKENS + SMALL_TOKENS + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_a",
                    &"x".repeat((READ_TOKENS as usize) * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b",
                    &"y".repeat((SMALL_TOKENS as usize) * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let files = aggregate_by_file(&result.attributions);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/big.ts");
        assert_eq!(files[1].path, "/small.ts");
        assert!(files[0].total_cost > files[1].total_cost);
    }

    #[test]
    fn aggregates_by_bash_args_hash_so_repeated_commands_collapse() {
        let pricing = load_builtin_pricing();
        let session_id = "s-bash";
        let mut turns: Vec<TurnRecord> = Vec::new();
        let mut ts = 0u64;
        for i in 0..3 {
            turns.push(turn(
                session_id,
                &format!("msg-emit-{i}"),
                ts,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc_with_hash(
                    &format!("tu_b_{i}"),
                    "Bash",
                    "ls -la",
                    "Bash:ls",
                )],
                SourceKind::ClaudeCode,
            ));
            ts += 1;
            turns.push(turn(
                session_id,
                &format!("msg-pay-{i}"),
                ts,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 1000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ));
            ts += 1;
        }
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_b_0",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b_1",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.200Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b_2",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.300Z",
                ),
            ],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let bash = aggregate_by_bash(&result.attributions);
        assert_eq!(bash.len(), 1);
        assert_eq!(bash[0].call_count, 3);
    }

    #[test]
    fn aggregates_bash_cost_by_normalized_verb_with_distinct_command_and_examples() {
        let attrs = vec![
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git diff src/a.ts", "git:diff:a", 5.0, 100.0, 10.0, 1),
            bash_attribution("git diff src/a.ts", "git:diff:a", 5.0, 100.0, 10.0, 1),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("pnpm run test", "pnpm:test", 4.0, 40.0, 8.0, 1),
        ];

        let verbs = aggregate_by_bash_verb(&attrs, parse_bash_command);
        assert_eq!(verbs[0].verb, "git diff");
        assert_eq!(verbs[0].call_count, 5);
        assert_eq!(verbs[0].distinct_commands, 2);
        assert!((verbs[0].total_cost - 31.0).abs() < 1e-9);
        assert!((verbs[0].initial_tokens - 500.0).abs() < 1e-9);
        assert!((verbs[0].persistence_tokens - 80.0).abs() < 1e-9);
        assert!((verbs[0].avg_persistence_turns - 1.6).abs() < 1e-9);
        assert_eq!(
            verbs[0].top_examples,
            vec!["git diff src/b.ts", "git diff src/a.ts"]
        );

        assert_eq!(verbs[1].verb, "git status");
        assert_eq!(verbs[1].call_count, 3);
        assert_eq!(verbs[1].distinct_commands, 1);
        assert_eq!(verbs[2].verb, "pnpm test");
    }

    #[test]
    fn aggregates_subagent_calls_by_subagent_type() {
        let pricing = load_builtin_pricing();
        let session_id = "s-agent";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc_with_hash(
                    "tu_a1",
                    "Agent",
                    "general-purpose",
                    "Agent:gp",
                )],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 2000,
                    output: 10,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_a1",
                &"z".repeat(8000),
                "2026-04-20T00:00:00.100Z",
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let subagents = aggregate_by_subagent(&result.attributions);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].subagent_type, "general-purpose");
        assert_eq!(subagents[0].call_count, 1);
        assert!(subagents[0].total_cost > 0.0);
    }

    fn mcp_attribution(tool_name: &str, total_cost: f64, riding_turns: u64) -> ToolAttribution {
        ToolAttribution {
            tool_use_id: format!("tu-{tool_name}"),
            tool_name: tool_name.into(),
            target: None,
            args_hash: format!("{tool_name}:0"),
            session_id: "s-mcp".into(),
            emit_turn_index: 0,
            emit_ts: "2026-04-20T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            subagent_type: None,
            result_tokens: 0,
            result_bytes_estimated: true,
            initial_cost: total_cost,
            initial_tokens: total_cost * 100.0,
            persistence_cost: 0.0,
            persistence_tokens: total_cost * 50.0,
            riding_turns,
            total_cost,
            output_bytes: None,
            output_truncated: None,
        }
    }

    #[test]
    fn aggregates_by_mcp_server_groups_by_server_segment_and_sorts_by_cost() {
        // Two MCP servers + a non-MCP tool + a malformed mcp__ name. The
        // non-MCP + malformed rows must NOT show up; the relaycast roll-up
        // must collapse all three relaycast tools into a single row with
        // top_tools sorted by cost desc.
        let attrs = vec![
            mcp_attribution("mcp__relaycast__send_dm", 2.0, 1),
            mcp_attribution("mcp__relaycast__send_dm", 1.5, 0),
            mcp_attribution("mcp__relaycast__list_channels", 0.5, 0),
            mcp_attribution("mcp__relaycast__react_to_message", 0.25, 0),
            mcp_attribution("mcp__github__get_file_contents", 1.0, 2),
            mcp_attribution("mcp__github__create_pull_request", 0.1, 0),
            // Non-MCP — must be skipped.
            mcp_attribution("Read", 99.0, 5),
            // Malformed: missing tool segment.
            mcp_attribution("mcp__only_server__", 50.0, 0),
            // Malformed: missing server segment.
            mcp_attribution("mcp____tool_only", 50.0, 0),
            // Malformed: not enough separators.
            mcp_attribution("mcp__no_double_separator", 50.0, 0),
        ];

        let rows = aggregate_by_mcp_server(&attrs);
        assert_eq!(
            rows.len(),
            2,
            "only the two well-formed mcp__ servers should aggregate"
        );

        // relaycast wins on cumulative cost (2.0 + 1.5 + 0.5 + 0.25 = 4.25)
        // vs github (1.0 + 0.1 = 1.1).
        let relaycast = &rows[0];
        assert_eq!(relaycast.server, "relaycast");
        assert_eq!(relaycast.call_count, 4);
        assert!((relaycast.total_cost - 4.25).abs() < 1e-9);
        assert!((relaycast.initial_tokens - 4.25 * 100.0).abs() < 1e-9);
        assert!((relaycast.persistence_tokens - 4.25 * 50.0).abs() < 1e-9);
        assert_eq!(relaycast.riding_turns, 1);
        assert_eq!(
            relaycast.top_tools,
            vec!["send_dm", "list_channels", "react_to_message"],
        );

        let github = &rows[1];
        assert_eq!(github.server, "github");
        assert_eq!(github.call_count, 2);
        assert!((github.total_cost - 1.1).abs() < 1e-9);
        assert_eq!(github.riding_turns, 2);
        assert_eq!(
            github.top_tools,
            vec!["get_file_contents", "create_pull_request"],
        );
    }

    #[test]
    fn falls_back_to_even_split_when_no_content_is_provided() {
        let pricing = load_builtin_pricing();
        let session_id = "s-fallback";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_x", "Read", Some("/a.ts")),
                    tc("tu_y", "Read", Some("/b.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 4000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        assert_eq!(result.attributions.len(), 2);
        let rate = pricing.get("claude-sonnet-4-6").unwrap();
        let expected = ((4000.0 / 1_000_000.0) * rate.input) / 2.0;
        for a in &result.attributions {
            assert!((a.initial_cost - expected).abs() < 1e-9);
            assert_eq!(a.persistence_cost, 0.0);
        }
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::EvenSplit
        );
    }

    #[test]
    fn uses_user_turn_block_sizes_when_content_sidecar_is_unavailable() {
        let pricing = load_builtin_pricing();
        let session_id = "s-user-turn-fallback";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_big", "Read", Some("/big.ts")),
                    tc("tu_small", "Read", Some("/small.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 4000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: 4500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];

        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            session_id.into(),
            vec![user_turn(
                session_id,
                "u-1",
                vec![
                    tool_result_block("tu_big", 12_000, 3000),
                    tool_result_block("tu_small", 4000, 1000),
                ],
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: Some(&user_turns_by_session),
                tool_result_events_by_session: None,
            },
        );
        let by_id: HashMap<&str, &ToolAttribution> = result
            .attributions
            .iter()
            .map(|a| (a.tool_use_id.as_str(), a))
            .collect();
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::Sized
        );
        assert!((by_id["tu_big"].initial_tokens - 3000.0).abs() < 1e-9);
        assert!((by_id["tu_small"].initial_tokens - 1000.0).abs() < 1e-9);
        assert!((by_id["tu_big"].persistence_tokens - 3000.0).abs() < 1e-9);
        assert!((by_id["tu_small"].persistence_tokens - 1000.0).abs() < 1e-9);
        assert!(by_id["tu_big"].total_cost > by_id["tu_small"].total_cost);
    }

    #[test]
    fn prefers_user_turn_block_sizes_over_content_sidecar_estimates() {
        let pricing = load_builtin_pricing();
        let session_id = "s-sidecar-primary";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc("tu_read", "Read", Some("/file.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 10_000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_read",
                &"x".repeat(1000 * 4),
                "2026-04-20T00:00:00.100Z",
            )],
        );
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            session_id.into(),
            vec![user_turn(
                session_id,
                "u-1",
                vec![tool_result_block("tu_read", 36_000, 9000)],
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: Some(&user_turns_by_session),
                tool_result_events_by_session: None,
            },
        );
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::Sized
        );
        assert!((result.attributions[0].initial_tokens - 9000.0).abs() < 1e-9);
    }

    #[test]
    fn caps_sibling_initial_cost_at_next_turns_actual_new_content() {
        let pricing = load_builtin_pricing();
        let session_id = "s-cap";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_big", "Read", Some("/big.ts")),
                    tc("tu_med", "Read", Some("/med.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 5000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_big",
                    &"x".repeat(6000 * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_med",
                    &"y".repeat(4000 * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let summed: f64 = result.attributions.iter().map(|a| a.initial_tokens).sum();
        assert!(summed <= 5000.0 + 1e-6, "summed={summed} > newContent=5000");
        let big = result
            .attributions
            .iter()
            .find(|a| a.tool_use_id == "tu_big")
            .unwrap();
        let med = result
            .attributions
            .iter()
            .find(|a| a.tool_use_id == "tu_med")
            .unwrap();
        assert!((big.initial_tokens - 3000.0).abs() < 1e-6);
        assert!((med.initial_tokens - 2000.0).abs() < 1e-6);
    }

    #[test]
    fn caps_sibling_persistence_at_turns_actual_cache_read() {
        let pricing = load_builtin_pricing();
        let session_id = "s-persist-cap";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_a", "Read", Some("/a.ts")),
                    tc("tu_b", "Read", Some("/b.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 8000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 50,
                    output: 5,
                    reasoning: 0,
                    cache_read: 5000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_a",
                    &"x".repeat(4000 * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b",
                    &"y".repeat(4000 * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let summed_persist: f64 = result
            .attributions
            .iter()
            .map(|a| a.persistence_tokens)
            .sum();
        assert!(
            summed_persist <= 5000.0 + 1e-6,
            "summedPersist={summed_persist} > cacheRead=5000"
        );
        for a in &result.attributions {
            assert!((a.persistence_tokens - 2500.0).abs() < 1e-6);
        }
    }

    #[test]
    fn uses_paying_turns_model_rate_not_emit_turns() {
        let pricing = load_builtin_pricing();
        let sonnet = pricing.get("claude-sonnet-4-6").unwrap().clone();
        let haiku = pricing.get("claude-haiku-4-5").unwrap().clone();
        assert_ne!(haiku.input, sonnet.input, "test prerequisite: rates differ");

        let session_id = "s-cross-model";
        const TOK: u64 = 4000;
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc("tu_x", "Read", Some("/x.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-haiku-4-5",
                Usage {
                    input: TOK,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-haiku-4-5",
                Usage {
                    input: 50,
                    output: 5,
                    reasoning: 0,
                    cache_read: TOK + 100,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_x",
                &"z".repeat((TOK as usize) * 4),
                "2026-04-20T00:00:00.100Z",
            )],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        let a = &result.attributions[0];
        let expected_initial = (TOK as f64 / 1_000_000.0) * haiku.input;
        let expected_persistence = (TOK as f64 / 1_000_000.0) * haiku.cache_read;
        assert!(
            (a.initial_cost - expected_initial).abs() < 1e-9,
            "initial_cost={} expected={}",
            a.initial_cost,
            expected_initial
        );
        assert!(
            (a.persistence_cost - expected_persistence).abs() < 1e-9,
            "persistence_cost={} expected={}",
            a.persistence_cost,
            expected_persistence
        );
    }

    #[test]
    fn session_grand_honors_source_aware_reasoning_for_codex() {
        // Regression: hotspots must use `cost_for_turn` so its `session_grand`
        // inherits Codex's `included_in_output` reasoning semantics. Otherwise
        // it overstates by `reasoning × output_rate` and drifts away from the
        // canonical `cost.rs` totals.
        let pricing = load_builtin_pricing();
        let codex_model = if pricing.contains_key("gpt-5-codex") {
            "gpt-5-codex"
        } else {
            "claude-sonnet-4-6"
        };
        let session_id = "s-codex-reasoning";
        let turns = vec![turn(
            session_id,
            "msg-0",
            0,
            "2026-04-20T00:00:00.000Z",
            codex_model,
            Usage {
                input: 1000,
                // Codex `output_tokens` already includes reasoning.
                output: 500,
                reasoning: 200,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![],
            SourceKind::Codex,
        )];
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );

        let rate = pricing.get(codex_model).unwrap();
        let expected = (1000.0 / 1_000_000.0) * rate.input + (500.0 / 1_000_000.0) * rate.output;
        assert!(
            (result.grand_total - expected).abs() < 1e-9,
            "Codex sessionGrand should not bill reasoning at output rate: got={} expected={}",
            result.grand_total,
            expected
        );
    }

    #[test]
    fn grand_total_plus_unattributed_equals_session_grand_within_rounding() {
        let pricing = load_builtin_pricing();
        let session_id = "s-totals";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 50,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![tc("tu_z", "Read", Some("/z.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 2000,
                    output: 30,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_z",
                &"q".repeat(2000 * 4),
                "2026-04-20T00:00:00.500Z",
            )],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
                tool_result_events_by_session: None,
            },
        );
        assert!(
            (result.attributed_total + result.unattributed_total - result.grand_total).abs() < 1e-9
        );
    }

    #[test]
    fn attribution_method_serializes_to_kebab_case() {
        // The CLI/MCP presenters round-trip these enums through JSON, so the
        // wire format must match the TS string union ('sized' | 'even-split').
        assert_eq!(
            serde_json::to_string(&AttributionMethod::Sized).unwrap(),
            "\"sized\""
        );
        assert_eq!(
            serde_json::to_string(&AttributionMethod::EvenSplit).unwrap(),
            "\"even-split\""
        );
    }

    /// Regression for #436: a 1 MB Bash result that gets truncated to a
    /// small token count must rank above a small-bytes / large-tokens
    /// Read when sorted by `total_output_bytes`. The bash row also has
    /// to flag `truncated_count > 0` from the propagated
    /// `output_truncated`.
    #[test]
    fn aggregations_track_output_bytes_so_byte_ranking_inverts_token_ranking() {
        use crate::reader::{ToolResultEventRecord, ToolResultEventSource, ToolResultStatus};

        let pricing = load_builtin_pricing();
        let session_id = "s-bytes";

        // Emit a Bash call and a Read call on turn 0. Turn 1 pays for
        // both. The Bash payload is 1 MB raw bytes but the user-turn
        // block reports a small post-truncation token count; the Read
        // payload is tiny but the user-turn block reports a large token
        // count. Token-sort puts Read first; byte-sort must put Bash
        // first.
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-05-25T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc_with_hash("tu_bash", "Bash", "find / -name foo", "Bash:find"),
                    tc("tu_read", "Read", Some("/big.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-05-25T00:00:01.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 5000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];

        // User-turn block sizes drive the token ranking: Read is "big"
        // in tokens (4000), Bash is "small" in tokens (200) because
        // Claude truncated it before tokenizing.
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            session_id.into(),
            vec![user_turn(
                session_id,
                "u-1",
                vec![
                    tool_result_block("tu_bash", 800, 200),
                    tool_result_block("tu_read", 16_000, 4000),
                ],
            )],
        );

        // Tool-result event payload sizes drive the byte ranking: Bash
        // is 1 MB (pre-token-truncation raw stdout), Read is 1 KB.
        const BASH_BYTES: u64 = 1_000_000;
        const READ_BYTES: u64 = 1_000;
        let bash_event = ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            message_id: Some("msg-0".into()),
            tool_use_id: "tu_bash".into(),
            call_index: Some(0),
            event_index: 0,
            ts: Some("2026-05-25T00:00:00.500Z".into()),
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: Some(BASH_BYTES),
            output_bytes: Some(BASH_BYTES),
            output_truncated: Some(true),
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        let read_event = ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            message_id: Some("msg-0".into()),
            tool_use_id: "tu_read".into(),
            call_index: Some(0),
            event_index: 1,
            ts: Some("2026-05-25T00:00:00.500Z".into()),
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: Some(READ_BYTES),
            output_bytes: Some(READ_BYTES),
            output_truncated: Some(false),
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        let mut events_by_session: HashMap<String, Vec<ToolResultEventRecord>> = HashMap::new();
        events_by_session.insert(session_id.into(), vec![bash_event, read_event]);

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: Some(&user_turns_by_session),
                tool_result_events_by_session: Some(&events_by_session),
            },
        );

        // Sanity: bytes / truncation rode through to ToolAttribution.
        let by_id: HashMap<&str, &ToolAttribution> = result
            .attributions
            .iter()
            .map(|a| (a.tool_use_id.as_str(), a))
            .collect();
        assert_eq!(by_id["tu_bash"].output_bytes, Some(BASH_BYTES));
        assert_eq!(by_id["tu_bash"].output_truncated, Some(true));
        assert_eq!(by_id["tu_read"].output_bytes, Some(READ_BYTES));
        assert_eq!(by_id["tu_read"].output_truncated, Some(false));

        // Token-driven cost ranks Read first (4000 tok > 200 tok).
        let files = aggregate_by_file(&result.attributions);
        assert_eq!(files.len(), 1, "Read is the only file-touching tool");
        let bash = aggregate_by_bash(&result.attributions);
        assert_eq!(bash.len(), 1);
        let read_file = &files[0];
        let bash_row = &bash[0];
        // The Read row out-costs the Bash row (sized attribution).
        assert!(
            read_file.total_cost > bash_row.total_cost,
            "expected Read cost > Bash cost in token-sized attribution; got read={} bash={}",
            read_file.total_cost,
            bash_row.total_cost,
        );

        // Bytes plumbing populated on both aggregations.
        assert_eq!(read_file.total_output_bytes, READ_BYTES);
        assert_eq!(read_file.max_output_bytes, READ_BYTES);
        assert_eq!(read_file.truncated_count, 0);
        assert_eq!(bash_row.total_output_bytes, BASH_BYTES);
        assert_eq!(bash_row.max_output_bytes, BASH_BYTES);
        assert_eq!(bash_row.truncated_count, 1);

        // Byte ranking inverts the cost ranking: Bash should win when
        // we sort by total_output_bytes. The SDK's default sort is by
        // cost; we just confirm the underlying counter inverts.
        assert!(
            bash_row.total_output_bytes > read_file.total_output_bytes,
            "byte ranking should put Bash (1 MB) ahead of Read (1 KB)"
        );
    }
