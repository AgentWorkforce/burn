//! Conformance tests for the subagent_tree module — extracted verbatim from the
//! former inline `#[cfg(test)] mod tests` block (included via `#[path]`).

    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{
        RelationshipSourceKind, RelationshipType, SourceKind, Subagent, ToolCall, TurnRecord, Usage,
    };

    fn make_turn(
        session_id: &str,
        message_id: &str,
        model: &str,
        turn_index: u64,
        source: SourceKind,
        subagent: Option<Subagent>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index,
            ts: "2026-04-20T00:00:00.000Z".into(),
            model: model.into(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 1000,
                output: 1000,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: Vec::<ToolCall>::new(),
            files_touched: None,
            subagent,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn sub(
        agent_id: Option<&str>,
        parent_agent_id: Option<&str>,
        subagent_type: Option<&str>,
        description: Option<&str>,
    ) -> Subagent {
        Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: agent_id.map(String::from),
            parent_agent_id: parent_agent_id.map(String::from),
            subagent_type: subagent_type.map(String::from),
            description: description.map(String::from),
        }
    }

    fn rel(
        session_id: &str,
        rel_type: RelationshipType,
        related: Option<&str>,
        agent_id: Option<&str>,
        subagent_type: Option<&str>,
        description: Option<&str>,
        source: RelationshipSourceKind,
    ) -> SessionRelationshipRecord {
        SessionRelationshipRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            related_session_id: related.map(String::from),
            relationship_type: rel_type,
            ts: None,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: agent_id.map(String::from),
            subagent_type: subagent_type.map(String::from),
            description: description.map(String::from),
        }
    }

    #[test]
    fn folds_cumulative_cost_from_nested_subagents_up_to_the_main_root() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-1";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "m2",
                "claude-sonnet-4-6",
                1,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "o1",
                "claude-haiku-4-5",
                2,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    Some("Research"),
                )),
            ),
            make_turn(
                session_id,
                "o2",
                "claude-haiku-4-5",
                3,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    None,
                )),
            ),
            make_turn(
                session_id,
                "i1",
                "claude-haiku-4-5",
                4,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-inner"),
                    Some("u-outer"),
                    Some("code-reviewer"),
                    None,
                )),
            ),
        ];

        let opts = BuildSubagentTreeOptions::new(&pricing);
        let trees = build_subagent_tree(&turns, &opts);
        let root = trees.get(session_id).expect("root");
        assert_eq!(root.label, "main");
        assert_eq!(root.depth, 0);
        assert_eq!(root.self_turns, 2);
        assert_eq!(root.cumulative_turns, 5);
        assert!(root.cumulative_cost > root.self_cost);

        assert_eq!(root.children.len(), 1);
        let outer = &root.children[0];
        assert_eq!(outer.label, "Explore");
        assert_eq!(outer.depth, 1);
        assert_eq!(outer.self_turns, 2);
        assert_eq!(outer.cumulative_turns, 3);
        assert_eq!(outer.children.len(), 1);

        let inner = &outer.children[0];
        assert_eq!(inner.label, "code-reviewer");
        assert_eq!(inner.depth, 2);
        assert_eq!(inner.self_turns, 1);
        assert_eq!(inner.cumulative_turns, 1);
        assert!((inner.cumulative_cost - inner.self_cost).abs() < 1e-12);

        assert!(
            (outer.cumulative_cost - (outer.self_cost + inner.cumulative_cost)).abs() < 1e-12,
            "outer cumulative is selfCost + inner.cumulativeCost"
        );
    }

    #[test]
    fn buckets_sidechain_turns_without_agent_id_under_an_unresolved_node() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-2";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "s1",
                "claude-haiku-4-5",
                1,
                SourceKind::ClaudeCode,
                Some(Subagent {
                    is_sidechain: true,
                    parent_tool_use_id: None,
                    agent_id: None,
                    parent_agent_id: None,
                    subagent_type: None,
                    description: None,
                }),
            ),
        ];
        let opts = BuildSubagentTreeOptions::new(&pricing);
        let trees = build_subagent_tree(&turns, &opts);
        let root = trees.get(session_id).unwrap();
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].label, "(unresolved)");
        assert_eq!(root.children[0].self_turns, 1);
    }

    #[test]
    fn builds_the_same_claude_tree_from_session_relationship_records() {
        let pricing = load_builtin_pricing();
        let session_id = "sess-graph";
        let turns = vec![
            make_turn(
                session_id,
                "m1",
                "claude-sonnet-4-6",
                0,
                SourceKind::ClaudeCode,
                None,
            ),
            make_turn(
                session_id,
                "o1",
                "claude-haiku-4-5",
                1,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-outer"),
                    Some(session_id),
                    Some("Explore"),
                    Some("Research"),
                )),
            ),
            make_turn(
                session_id,
                "i1",
                "claude-haiku-4-5",
                2,
                SourceKind::ClaudeCode,
                Some(sub(
                    Some("u-inner"),
                    Some("u-outer"),
                    Some("code-reviewer"),
                    None,
                )),
            ),
        ];
        let relationships = vec![
            rel(
                session_id,
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::ClaudeCode,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some(session_id),
                Some("u-outer"),
                Some("Explore"),
                Some("Research"),
                RelationshipSourceKind::NativeClaude,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some("u-outer"),
                Some("u-inner"),
                Some("code-reviewer"),
                None,
                RelationshipSourceKind::NativeClaude,
            ),
        ];

        let legacy_opts = BuildSubagentTreeOptions::new(&pricing);
        let legacy = build_subagent_tree(&turns, &legacy_opts)
            .get(session_id)
            .unwrap()
            .clone();
        let graph_opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let graph = build_subagent_tree(&turns, &graph_opts)
            .get(session_id)
            .unwrap()
            .clone();
        assert_eq!(graph, legacy);
        assert_eq!(graph.relationship_type, RelationshipType::Root);
        assert_eq!(
            graph.children[0].relationship_type,
            RelationshipType::Subagent
        );
    }

    #[test]
    fn joins_child_session_relationship_rows_to_turns_without_per_turn_subagent_metadata() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            make_turn(
                "parent-session",
                "parent-1",
                "gpt-5.1-codex",
                0,
                SourceKind::Codex,
                None,
            ),
            make_turn(
                "child-session",
                "child-1",
                "gpt-5.1-codex",
                0,
                SourceKind::Codex,
                None,
            ),
        ];
        let relationships = vec![
            rel(
                "parent-session",
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::Codex,
            ),
            rel(
                "child-session",
                RelationshipType::Subagent,
                Some("parent-session"),
                Some("agent-child"),
                Some("worker"),
                None,
                RelationshipSourceKind::Codex,
            ),
        ];

        let opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let root = build_subagent_tree(&turns, &opts)
            .get("parent-session")
            .unwrap()
            .clone();
        assert_eq!(root.self_turns, 1);
        assert_eq!(root.cumulative_turns, 2);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].label, "worker");
        assert_eq!(root.children[0].node_id, "child-session");
        assert_eq!(
            root.children[0].relationship_type,
            RelationshipType::Subagent
        );
        assert_eq!(root.children[0].self_turns, 1);
    }

    #[test]
    fn does_not_alias_native_sidechain_session_roots_onto_agent_ids_when_turns_lack_subagent_fields(
    ) {
        let pricing = load_builtin_pricing();
        let session_id = "partial-claude";
        let turns = vec![make_turn(
            session_id,
            "main-1",
            "claude-sonnet-4-6",
            0,
            SourceKind::ClaudeCode,
            None,
        )];
        let relationships = vec![
            rel(
                session_id,
                RelationshipType::Root,
                None,
                None,
                None,
                None,
                RelationshipSourceKind::ClaudeCode,
            ),
            rel(
                session_id,
                RelationshipType::Subagent,
                Some(session_id),
                Some("u-outer"),
                Some("Explore"),
                None,
                RelationshipSourceKind::NativeClaude,
            ),
        ];
        let opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
        let root = build_subagent_tree(&turns, &opts)
            .get(session_id)
            .unwrap()
            .clone();
        assert_eq!(root.node_id, session_id);
        assert_eq!(root.label, "main");
        assert_eq!(root.self_turns, 1);
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].node_id, "u-outer");
        assert_eq!(root.children[0].self_turns, 0);
    }

    #[test]
    fn reports_median_p95_mean_total_per_subagent_type_across_invocations() {
        let pricing = load_builtin_pricing();
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            let agent_id = format!("u-exp-{i}");
            for j in 0..=i {
                turns.push(make_turn(
                    &format!("sess-{i}"),
                    &format!("m-{i}-{j}"),
                    "claude-haiku-4-5",
                    j as u64,
                    SourceKind::ClaudeCode,
                    Some(sub(Some(&agent_id), None, Some("Explore"), None)),
                ));
            }
        }
        turns.push(make_turn(
            "sess-rev",
            "mr",
            "claude-haiku-4-5",
            0,
            SourceKind::ClaudeCode,
            Some(sub(Some("u-rev"), None, Some("code-reviewer"), None)),
        ));

        let opts = BuildSubagentTreeOptions::new(&pricing);
        let stats = aggregate_subagent_type_stats(&turns, &opts);
        let explore = stats.iter().find(|s| s.subagent_type == "Explore").unwrap();
        assert_eq!(explore.invocations, 3);
        assert_eq!(explore.turns, 6);
        assert!(explore.median_cost > 0.0);
        assert!(explore.p95_cost >= explore.median_cost);
        assert!((explore.mean_cost - explore.total_cost / 3.0).abs() < 1e-12);

        let rev = stats
            .iter()
            .find(|s| s.subagent_type == "code-reviewer")
            .unwrap();
        assert_eq!(rev.invocations, 1);
        assert_eq!(rev.turns, 1);
        assert!((rev.median_cost - rev.total_cost).abs() < 1e-12);
        assert!((rev.p95_cost - rev.total_cost).abs() < 1e-12);
    }
