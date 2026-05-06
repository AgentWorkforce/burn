//! `compare` from the SQLite-backed ledger — Rust port of
//! `packages/analyze/src/compare-archive.ts`.
//!
//! Per #259's redesign, the Rust ledger is already SQLite-only, so this
//! port is a thin shell over [`Ledger::query_turns`] + [`build_compare_table`].
//! The TS path's bespoke per-column SQL aggregation existed because the
//! 1.x JSONL ledger had no typed columns; the 2.x port does, so we get
//! the same per-cell parity for free by reusing the in-memory primitive.
//!
//! `analyzed_turns` is the pre-`models`-filter count — matches the TS
//! semantics that derive it from `queryAll(q).length` rather than from the
//! post-filter table.

use crate::ledger::{Ledger, Query, Result as LedgerResult};

use crate::analyze::compare::{build_compare_table, CompareOptions, CompareTable};

#[derive(Debug, Clone, PartialEq)]
pub struct CompareFromArchiveResult {
    pub table: CompareTable,
    /// Total turn count (pre-`models` filter) matching `q`. Mirrors the TS
    /// path's `analyzedTurns`, which is sourced from `queryAll(q).length`.
    pub analyzed_turns: usize,
}

/// Build a [`CompareTable`] sourced from the SQLite ledger. The filter
/// pipeline (since / until / project / session_id / source) is applied
/// inside [`Ledger::query_turns`]; the model allow-list lives in `opts`
/// and is honored by [`build_compare_table`].
pub fn compare_from_archive(
    ledger: &Ledger,
    q: &Query,
    opts: &CompareOptions<'_>,
) -> LedgerResult<CompareFromArchiveResult> {
    let turns = ledger.query_turns(q)?;
    let analyzed_turns = turns.len();
    let table = build_compare_table(&turns, opts);
    Ok(CompareFromArchiveResult {
        table,
        analyzed_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::compare::CompareOptions;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::ledger::{Ledger, LedgerLayout, Query};
    use crate::reader::{ActivityCategory, SourceKind, ToolCall, TurnRecord, Usage};
    use tempfile::TempDir;

    fn open_in(tmp: &TempDir) -> Ledger {
        let layout = LedgerLayout::under(tmp.path());
        Ledger::open(&layout.burn, &layout.content).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn fake_turn(
        session: &str,
        message: &str,
        ts: &str,
        model: &str,
        activity: Option<ActivityCategory>,
        has_edits: Option<bool>,
        retries: Option<u64>,
        usage: Usage,
        source: SourceKind,
        project: Option<&str>,
        project_key: Option<&str>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session.into(),
            session_path: None,
            message_id: message.into(),
            turn_index: 0,
            ts: ts.into(),
            model: model.into(),
            project: project.map(str::to_string),
            project_key: project_key.map(str::to_string),
            usage,
            tool_calls: Vec::<ToolCall>::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity,
            retries,
            has_edits,
            fidelity: None,
        }
    }

    fn default_usage(input: u64) -> Usage {
        Usage {
            input,
            output: 500,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn assert_num_near(a: Option<f64>, b: Option<f64>, msg: &str) {
        match (a, b) {
            (None, None) => {}
            (Some(x), Some(y)) => {
                assert!((x - y).abs() < 1e-9, "{msg}: {x} != {y}");
            }
            other => panic!("{msg}: mismatched optionals: {:?}", other),
        }
    }

    #[test]
    fn parity_matches_in_memory_build_compare_table_for_a_mixed_fixture() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        let mut turns: Vec<TurnRecord> = Vec::new();
        let mut mid = 0u64;
        let mut next_id = || {
            mid += 1;
            format!("m-{mid}")
        };

        // 6 Sonnet coding turns, 4 one-shot.
        for i in 0..4 {
            turns.push(fake_turn(
                "s-sonnet",
                &next_id(),
                &format!("2026-04-20T00:00:{:02}.000Z", i),
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                Some(true),
                Some(0),
                Usage {
                    input: 5000,
                    output: 800,
                    reasoning: 0,
                    cache_read: 12000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                SourceKind::ClaudeCode,
                Some("/tmp/project"),
                None,
            ));
        }
        turns.push(fake_turn(
            "s-sonnet",
            &next_id(),
            "2026-04-20T00:00:04.000Z",
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            Some(true),
            Some(2),
            default_usage(1000),
            SourceKind::ClaudeCode,
            Some("/tmp/project"),
            None,
        ));
        turns.push(fake_turn(
            "s-sonnet",
            &next_id(),
            "2026-04-20T00:00:05.000Z",
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            Some(true),
            Some(1),
            default_usage(1100),
            SourceKind::ClaudeCode,
            Some("/tmp/project"),
            None,
        ));

        // 5 Haiku coding turns.
        for i in 0..5u64 {
            turns.push(fake_turn(
                "s-haiku",
                &next_id(),
                &format!("2026-04-20T01:00:{:02}.000Z", i),
                "claude-haiku-4-5",
                Some(ActivityCategory::Coding),
                Some(true),
                Some(if i < 2 { 0 } else { i }),
                Usage {
                    input: 2000,
                    output: 400,
                    reasoning: 0,
                    cache_read: if i % 2 == 0 { 6000 } else { 0 },
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                SourceKind::ClaudeCode,
                Some("/tmp/project"),
                None,
            ));
        }

        // Sonnet exploration (no edits).
        turns.push(fake_turn(
            "s-expl",
            &next_id(),
            "2026-04-20T02:00:00.000Z",
            "claude-sonnet-4-6",
            Some(ActivityCategory::Exploration),
            Some(false),
            None,
            default_usage(1200),
            SourceKind::ClaudeCode,
            Some("/tmp/project"),
            None,
        ));
        turns.push(fake_turn(
            "s-expl",
            &next_id(),
            "2026-04-20T02:00:01.000Z",
            "claude-sonnet-4-6",
            Some(ActivityCategory::Exploration),
            Some(false),
            None,
            default_usage(1300),
            SourceKind::ClaudeCode,
            Some("/tmp/project"),
            None,
        ));

        // Unpriced model.
        turns.push(fake_turn(
            "s-unpriced",
            &next_id(),
            "2026-04-20T03:00:00.000Z",
            "definitely-not-a-model",
            Some(ActivityCategory::Coding),
            Some(true),
            Some(1),
            default_usage(1400),
            SourceKind::ClaudeCode,
            Some("/tmp/project"),
            None,
        ));

        // Codex source — exercises the source-aware reasoning override
        // baked into `cost_for_turn`.
        turns.push(fake_turn(
            "s-codex",
            &next_id(),
            "2026-04-20T04:00:00.000Z",
            "gpt-5-codex",
            Some(ActivityCategory::Coding),
            Some(true),
            Some(0),
            Usage {
                input: 10000,
                output: 2000,
                reasoning: 800,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            SourceKind::Codex,
            Some("/tmp/project"),
            None,
        ));

        ledger.append_turns(&turns).unwrap();

        let opts = CompareOptions {
            pricing: &pricing,
            models: None,
            min_sample: Some(5),
        };
        let in_memory_turns = ledger.query_turns(&Query::default()).unwrap();
        let in_memory = crate::analyze::compare::build_compare_table(&in_memory_turns, &opts);
        let from_archive = compare_from_archive(&ledger, &Query::default(), &opts).unwrap();

        assert_eq!(from_archive.table.models, in_memory.models, "models order");
        assert_eq!(
            from_archive.table.categories, in_memory.categories,
            "categories order"
        );
        assert_eq!(from_archive.table.min_sample, in_memory.min_sample);
        assert_eq!(
            from_archive.analyzed_turns,
            in_memory_turns.len(),
            "analyzed_turns"
        );

        for m in &in_memory.models {
            for cat in &in_memory.categories {
                let a = &from_archive.table.cells[m][cat];
                let b = &in_memory.cells[m][cat];
                assert_eq!(a.turns, b.turns, "{m}/{cat} turns");
                assert_eq!(a.edit_turns, b.edit_turns, "{m}/{cat} edit_turns");
                assert_eq!(
                    a.one_shot_turns, b.one_shot_turns,
                    "{m}/{cat} one_shot_turns"
                );
                assert_eq!(a.priced_turns, b.priced_turns, "{m}/{cat} priced_turns");
                assert_num_near(
                    Some(a.total_cost),
                    Some(b.total_cost),
                    &format!("{m}/{cat} total_cost"),
                );
                assert_num_near(
                    a.cost_per_turn,
                    b.cost_per_turn,
                    &format!("{m}/{cat} cost_per_turn"),
                );
                assert_num_near(
                    a.one_shot_rate,
                    b.one_shot_rate,
                    &format!("{m}/{cat} one_shot_rate"),
                );
                assert_num_near(
                    a.cache_hit_rate,
                    b.cache_hit_rate,
                    &format!("{m}/{cat} cache_hit_rate"),
                );
                assert_eq!(
                    a.median_retries, b.median_retries,
                    "{m}/{cat} median_retries"
                );
                assert_eq!(a.no_data, b.no_data, "{m}/{cat} no_data");
                assert_eq!(
                    a.insufficient_sample, b.insufficient_sample,
                    "{m}/{cat} insufficient_sample"
                );
            }
        }

        for m in &in_memory.models {
            let a = &from_archive.table.totals[m];
            let b = &in_memory.totals[m];
            assert_eq!(a.turns, b.turns, "{m} totals.turns");
            assert_num_near(
                Some(a.total_cost),
                Some(b.total_cost),
                &format!("{m} totals.total_cost"),
            );
        }
    }

    #[test]
    fn honors_models_filter_and_pre_seeds_requested_but_absent_models() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-1",
                    "m-1",
                    "2026-04-20T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    Some(0),
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
                fake_turn(
                    "s-2",
                    "m-2",
                    "2026-04-20T00:00:01.000Z",
                    "claude-opus-4-7",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    Some(0),
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
            ])
            .unwrap();

        let opts = CompareOptions {
            pricing: &pricing,
            models: Some(vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()]),
            min_sample: None,
        };
        let result = compare_from_archive(&ledger, &Query::default(), &opts).unwrap();

        let mut sorted = result.table.models.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["claude-haiku-4-5", "claude-sonnet-4-6"]);
        assert!(result.table.cells["claude-haiku-4-5"]["coding"].no_data);
        assert_eq!(result.table.totals["claude-haiku-4-5"].turns, 0);
        // analyzed_turns is the pre-`models` count and must include both
        // ledger turns.
        assert_eq!(result.analyzed_turns, 2);
    }

    #[test]
    fn honors_since_filter() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-1",
                    "m-old",
                    "2026-04-19T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    Some(0),
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
                fake_turn(
                    "s-2",
                    "m-new",
                    "2026-04-21T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    Some(0),
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
            ])
            .unwrap();

        let q = Query {
            since: Some("2026-04-20T00:00:00.000Z".into()),
            ..Query::default()
        };
        let opts = CompareOptions::new(&pricing);
        let result = compare_from_archive(&ledger, &q, &opts).unwrap();
        assert_eq!(result.analyzed_turns, 1);
        assert_eq!(result.table.cells["claude-sonnet-4-6"]["coding"].turns, 1);
    }

    #[test]
    fn honors_project_filter_against_either_path_or_key() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-1",
                    "m-a",
                    "2026-04-20T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    None,
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    Some("/tmp/proj-a"),
                    Some("github.com/me/a"),
                ),
                fake_turn(
                    "s-2",
                    "m-b",
                    "2026-04-20T00:00:01.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    None,
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    Some("/tmp/proj-b"),
                    Some("github.com/me/b"),
                ),
            ])
            .unwrap();

        let opts = CompareOptions::new(&pricing);
        let by_path = compare_from_archive(
            &ledger,
            &Query {
                project: Some("/tmp/proj-a".into()),
                ..Query::default()
            },
            &opts,
        )
        .unwrap();
        assert_eq!(by_path.analyzed_turns, 1);

        let by_key = compare_from_archive(
            &ledger,
            &Query {
                project: Some("github.com/me/b".into()),
                ..Query::default()
            },
            &opts,
        )
        .unwrap();
        assert_eq!(by_key.analyzed_turns, 1);
    }

    #[test]
    fn honors_session_filter() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-X",
                    "m-x",
                    "2026-04-20T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    None,
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
                fake_turn(
                    "s-Y",
                    "m-y",
                    "2026-04-20T00:00:01.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Coding),
                    Some(true),
                    None,
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
            ])
            .unwrap();

        let opts = CompareOptions::new(&pricing);
        let result = compare_from_archive(&ledger, &Query::for_session("s-X"), &opts).unwrap();
        assert_eq!(result.analyzed_turns, 1);
    }

    #[test]
    fn honors_min_sample_flagging() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-1",
                    "m-1",
                    "2026-04-20T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Refactoring),
                    Some(true),
                    None,
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
                fake_turn(
                    "s-2",
                    "m-2",
                    "2026-04-20T00:00:01.000Z",
                    "claude-sonnet-4-6",
                    Some(ActivityCategory::Refactoring),
                    Some(true),
                    None,
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
            ])
            .unwrap();

        let opts = CompareOptions {
            pricing: &pricing,
            models: None,
            min_sample: Some(5),
        };
        let result = compare_from_archive(&ledger, &Query::default(), &opts).unwrap();
        let cell = &result.table.cells["claude-sonnet-4-6"]["refactoring"];
        assert_eq!(cell.turns, 2);
        assert!(cell.insufficient_sample);
        assert!(!cell.no_data);
    }

    #[test]
    fn empty_archive_yields_empty_table() {
        let tmp = TempDir::new().unwrap();
        let ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        let result =
            compare_from_archive(&ledger, &Query::default(), &CompareOptions::new(&pricing))
                .unwrap();
        assert_eq!(result.analyzed_turns, 0);
        assert!(result.table.models.is_empty());
        assert!(result.table.categories.is_empty());
        assert!(result.table.totals.is_empty());
    }

    #[test]
    fn single_cell_archive_populates_exactly_one_cell() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[fake_turn(
                "s-only",
                "m-only",
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                Some(true),
                Some(0),
                default_usage(1000),
                SourceKind::ClaudeCode,
                None,
                None,
            )])
            .unwrap();

        let result =
            compare_from_archive(&ledger, &Query::default(), &CompareOptions::new(&pricing))
                .unwrap();
        assert_eq!(result.table.models, vec!["claude-sonnet-4-6"]);
        assert_eq!(result.table.categories, vec!["coding"]);
        let cell = &result.table.cells["claude-sonnet-4-6"]["coding"];
        assert_eq!(cell.turns, 1);
        assert_eq!(cell.edit_turns, 1);
        assert_eq!(cell.one_shot_turns, 1);
        assert_eq!(cell.median_retries, Some(0.0));
        assert!(!cell.no_data);
        // Default min_sample (5) makes this insufficient — documented
        // behavior; the cell still reports its metrics, just flagged.
        assert!(cell.insufficient_sample);
    }

    #[test]
    fn groups_unclassified_turns_under_unclassified() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[
                fake_turn(
                    "s-1",
                    "m-u1",
                    "2026-04-20T00:00:00.000Z",
                    "claude-sonnet-4-6",
                    None,
                    None,
                    None,
                    default_usage(1000),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
                fake_turn(
                    "s-2",
                    "m-u2",
                    "2026-04-20T00:00:01.000Z",
                    "claude-sonnet-4-6",
                    None,
                    None,
                    None,
                    default_usage(1100),
                    SourceKind::ClaudeCode,
                    None,
                    None,
                ),
            ])
            .unwrap();

        let result =
            compare_from_archive(&ledger, &Query::default(), &CompareOptions::new(&pricing))
                .unwrap();
        assert!(result.table.categories.iter().any(|c| c == "unclassified"));
        assert_eq!(
            result.table.cells["claude-sonnet-4-6"]["unclassified"].turns,
            2
        );
    }

    #[test]
    fn codex_turns_bill_reasoning_as_included_in_output() {
        // Regression guard: Codex's `output_tokens` already includes
        // reasoning, so a Codex turn with reasoning_tokens > 0 must NOT
        // pay reasoning on top. The archive path delegates costing to
        // `cost_for_turn`, which honors the per-source override.
        let tmp = TempDir::new().unwrap();
        let mut ledger = open_in(&tmp);
        let pricing = load_builtin_pricing();

        ledger
            .append_turns(&[fake_turn(
                "s-cx",
                "m-cx",
                "2026-04-20T00:00:00.000Z",
                "gpt-5-codex",
                Some(ActivityCategory::Coding),
                Some(true),
                Some(0),
                Usage {
                    input: 10000,
                    output: 2000,
                    reasoning: 800,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                SourceKind::Codex,
                None,
                None,
            )])
            .unwrap();

        let opts = CompareOptions::new(&pricing);
        let in_memory_turns = ledger.query_turns(&Query::default()).unwrap();
        let in_memory = crate::analyze::compare::build_compare_table(&in_memory_turns, &opts);
        let from_archive = compare_from_archive(&ledger, &Query::default(), &opts).unwrap();

        let expected = in_memory
            .cells
            .get("gpt-5-codex")
            .and_then(|by_cat| by_cat.get("coding"))
            .map(|c| c.total_cost)
            .unwrap_or(0.0);
        let got = from_archive
            .table
            .cells
            .get("gpt-5-codex")
            .and_then(|by_cat| by_cat.get("coding"))
            .map(|c| c.total_cost)
            .unwrap_or(0.0);
        assert_num_near(Some(got), Some(expected), "Codex reasoning-mode parity");
    }
}
