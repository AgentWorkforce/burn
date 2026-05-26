//! Turn grouping by `parentUuid` chain walk.
//!
//! Claude Code's `~/.claude/projects/<key>/<sessionId>.jsonl` records carry a
//! `parentUuid` field that links each row to its causal predecessor. The
//! historical heuristic in the Claude reader keyed user→assistant text
//! association off file order ("the last user line seen before this
//! assistant"). That works for tidy sessions but mis-attributes text under:
//!
//! - **Mid-stream interruptions.** A user cancels mid-stream and re-prompts
//!   later; the resumed assistant rows still belong to the *original* user
//!   prompt by `parentUuid`, but file order puts a different user prompt
//!   between them.
//! - **Out-of-order JSONL flushes.** Claude Code's writer is async, so rows
//!   occasionally land slightly out of timestamp/file order.
//! - **Compaction artifacts.** Synthetic rows inserted at compaction time
//!   have timestamps that don't sit cleanly inside any turn window.
//!
//! The fix is the agent-profiler trick: walk `parentUuid` upward from each
//! row until you hit the nearest user-prompt ancestor; that ancestor's UUID
//! is the turn key. Out-of-order rows still hash into the right bucket
//! because the parent pointer is a content-defined link, not a position.
//!
//! ## Asymmetry note
//!
//! Codex rollouts (`crates/relayburn-sdk/src/reader/codex.rs`) and opencode
//! storage trees (`crates/relayburn-sdk/src/reader/opencode.rs`) do **not**
//! carry an equivalent `parentUuid`-style chain field. Both of those parsers
//! group turns by other primitives (Codex: `task_complete` boundaries; opencode:
//! per-`messageId` part files sorted chronologically) and are unaffected by
//! this module. The helpers here are Claude-specific and live under the
//! `claude/` submodule for that reason.
//!
//! See AgentWorkforce/burn#433.

use std::collections::{HashMap, HashSet};

/// Minimal accessor surface needed to walk a `parentUuid` chain. The Claude
/// reader's internal `LineNode` implements this trait; tests build a small
/// fixture struct that implements the same trait.
///
/// `uuid()` and `parent_uuid()` use `&str` returns so callers don't pay an
/// allocation per row. `is_user_root()` returns true when this row should
/// terminate the upward walk: i.e. the row is a user-prompt line (real user
/// text, not a harness-injected `<task-notification>` row, not a tool-result
/// envelope). The Claude reader decides what counts as a "user root" using
/// its own per-row inspection — this trait just exposes the boolean.
pub trait ChainNode {
    fn uuid(&self) -> &str;
    fn parent_uuid(&self) -> Option<&str>;
    fn is_user_root(&self) -> bool;
}

/// Build a `HashMap` from each `parentUuid` chain root (the nearest
/// user-prompt ancestor's UUID, or the row's own UUID when no user-prompt
/// ancestor exists) to the borrowed rows under that root.
///
/// Rows are visited in input order; per-bucket order matches input order.
///
/// **Cycle guard.** The walk uses a per-row visited set. If the parent
/// chain ever loops back on itself (corrupted writer, manual fixture
/// tampering), the walk terminates at the loop point and uses the
/// loop-entry row as the bucket key — better than hanging.
///
/// **Fallback for rows missing both `uuid` and `parentUuid`.** Rows whose
/// `uuid()` is empty cannot be walked or bucketed; they are skipped here.
/// The caller is responsible for any time-window / order-based fallback for
/// those rows. See the call site in `super::run_incremental` for how the
/// Claude reader carries the prior text association as a fallback for
/// legacy/malformed rows.
///
/// **Why this lives next to `super::nearest_user_prompt_root`.** The
/// Claude reader doesn't build the full bucket map at runtime — it only
/// needs the root UUID per assistant turn to look up user-prompt text
/// for classification, which the lighter `nearest_user_prompt_root`
/// helper covers. This bulk helper is the API surface the issue (#433)
/// specifies and is unit-tested below; downstream consumers that want
/// per-turn row sets (span trees, future inference rebuild) call this
/// version.
#[allow(dead_code)]
pub fn group_by_parent_chain<R: ChainNode>(rows: &[R]) -> HashMap<String, Vec<&R>> {
    if rows.is_empty() {
        return HashMap::new();
    }
    let by_uuid: HashMap<&str, &R> = rows
        .iter()
        .filter(|r| !r.uuid().is_empty())
        .map(|r| (r.uuid(), r))
        .collect();
    let mut out: HashMap<String, Vec<&R>> = HashMap::new();
    for row in rows {
        if row.uuid().is_empty() {
            continue;
        }
        let root = find_turn_root(row, &by_uuid);
        out.entry(root).or_default().push(row);
    }
    out
}

/// Walk upward from `row` along `parent_uuid()` until either:
///   1. A `ChainNode` reporting `is_user_root() == true` is found — return
///      that node's UUID.
///   2. The chain terminates (no parent_uuid, or parent_uuid not present
///      in the map) — return the deepest row's UUID we managed to reach.
///   3. A cycle is detected — return the row that closed the cycle.
///
/// Case (1) is the happy path the issue motivates. Cases (2) and (3) are
/// the "prefer over-grouping to dropping rows" branch from the issue
/// acceptance criteria: every row gets *some* bucket key, even when the
/// chain is incomplete.
#[allow(dead_code)]
pub fn find_turn_root<R: ChainNode>(start: &R, by_uuid: &HashMap<&str, &R>) -> String {
    if start.is_user_root() {
        return start.uuid().to_string();
    }
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(start.uuid());
    let mut current: &R = start;
    loop {
        let parent_uuid = match current.parent_uuid() {
            Some(p) if !p.is_empty() => p,
            _ => return current.uuid().to_string(),
        };
        // Cycle: parent points back to a row already on the path. Stop and
        // use the current row's UUID as the bucket key — see issue #433's
        // "prefer over-grouping" guidance.
        if !visited.insert(parent_uuid) {
            return current.uuid().to_string();
        }
        let parent = match by_uuid.get(parent_uuid) {
            Some(p) => *p,
            None => return current.uuid().to_string(),
        };
        if parent.is_user_root() {
            return parent.uuid().to_string();
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory row for testing the chain walker in isolation.
    /// The real Claude reader's `LineNode` implements `ChainNode` against
    /// its richer payload (sidechain flag, agent tool-use info, etc.).
    #[derive(Debug, Clone)]
    struct TestRow {
        uuid: String,
        parent_uuid: Option<String>,
        is_user: bool,
    }

    impl ChainNode for TestRow {
        fn uuid(&self) -> &str {
            &self.uuid
        }
        fn parent_uuid(&self) -> Option<&str> {
            self.parent_uuid.as_deref()
        }
        fn is_user_root(&self) -> bool {
            self.is_user
        }
    }

    fn user(uuid: &str, parent: Option<&str>) -> TestRow {
        TestRow {
            uuid: uuid.into(),
            parent_uuid: parent.map(str::to_string),
            is_user: true,
        }
    }

    fn asst(uuid: &str, parent: &str) -> TestRow {
        TestRow {
            uuid: uuid.into(),
            parent_uuid: Some(parent.into()),
            is_user: false,
        }
    }

    #[test]
    fn empty_input_yields_empty_map() {
        let rows: Vec<TestRow> = Vec::new();
        assert!(group_by_parent_chain(&rows).is_empty());
    }

    #[test]
    fn single_turn_simple_chain() {
        let rows = vec![
            user("u1", None),
            asst("a1", "u1"),
            asst("a2", "a1"),
            asst("a3", "a2"),
        ];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.len(), 1, "all rows should land under one root");
        let bucket = g.get("u1").unwrap();
        let ids: Vec<&str> = bucket.iter().map(|r| r.uuid()).collect();
        assert_eq!(ids, vec!["u1", "a1", "a2", "a3"]);
    }

    #[test]
    fn two_turns_grouped_by_distinct_roots() {
        let rows = vec![
            user("u1", None),
            asst("a1", "u1"),
            user("u2", Some("a1")),
            asst("a2", "u2"),
        ];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.len(), 2);
        let t1: Vec<&str> = g.get("u1").unwrap().iter().map(|r| r.uuid()).collect();
        assert_eq!(t1, vec!["u1", "a1"]);
        let t2: Vec<&str> = g.get("u2").unwrap().iter().map(|r| r.uuid()).collect();
        assert_eq!(t2, vec!["u2", "a2"]);
    }

    /// Out-of-order JSONL flush: the assistant rows for turn 1 and turn 2
    /// interleave in file order, but each row's `parentUuid` still points
    /// at the correct ancestor. The chain walk must group them by
    /// causal lineage, not file position.
    #[test]
    fn out_of_order_rows_group_by_chain_not_file_order() {
        let rows = vec![
            user("u1", None),
            user("u2", Some("a1_first")), // u2 chains off turn-1's first assistant
            asst("a1_first", "u1"),
            asst("a2_first", "u2"),
            asst("a1_second", "a1_first"), // arrives AFTER turn-2 starts!
            asst("a2_second", "a2_first"),
            asst("a1_third", "a1_second"), // even later
        ];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.len(), 2, "two turns despite interleaving");
        let mut t1: Vec<&str> = g.get("u1").unwrap().iter().map(|r| r.uuid()).collect();
        t1.sort();
        assert_eq!(t1, vec!["a1_first", "a1_second", "a1_third", "u1"]);
        let mut t2: Vec<&str> = g.get("u2").unwrap().iter().map(|r| r.uuid()).collect();
        t2.sort();
        assert_eq!(t2, vec!["a2_first", "a2_second", "u2"]);
    }

    /// Interrupt + resume pattern: the user cancels turn 1 mid-stream, types
    /// turn 2, then turn-1's late-arriving assistant chunks land last (wall
    /// clock gap between turn-1's first and last assistant rows is large).
    /// File-order grouping would attach the late chunks to turn 2; the
    /// chain walk attaches them to turn 1 because `parentUuid` still
    /// points up the original chain.
    #[test]
    fn interrupt_resume_groups_late_rows_with_original_turn() {
        let rows = vec![
            user("u_orig", None),
            asst("a_orig_1", "u_orig"),
            // user interrupts — types a new prompt that chains off a_orig_1
            user("u_resume", Some("a_orig_1")),
            asst("a_resume_1", "u_resume"),
            asst("a_resume_2", "a_resume_1"),
            // ...and only NOW does the original assistant's final chunk
            // arrive, with a very late wall-clock timestamp.
            asst("a_orig_late", "a_orig_1"),
        ];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.len(), 2);
        let mut t_orig: Vec<&str> = g.get("u_orig").unwrap().iter().map(|r| r.uuid()).collect();
        t_orig.sort();
        assert_eq!(t_orig, vec!["a_orig_1", "a_orig_late", "u_orig"]);
        let mut t_resume: Vec<&str> = g
            .get("u_resume")
            .unwrap()
            .iter()
            .map(|r| r.uuid())
            .collect();
        t_resume.sort();
        assert_eq!(t_resume, vec!["a_resume_1", "a_resume_2", "u_resume"]);
    }

    /// Cycle guard: synthetic loop in the parent chain must not hang the
    /// walker. The exact bucket key for a cycled row is an implementation
    /// detail (we use the row that closed the cycle), but the call must
    /// return in finite time and every row must land in some bucket.
    #[test]
    fn cycle_guard_terminates() {
        // a -> b -> a (cycle)
        let rows = vec![
            TestRow {
                uuid: "a".into(),
                parent_uuid: Some("b".into()),
                is_user: false,
            },
            TestRow {
                uuid: "b".into(),
                parent_uuid: Some("a".into()),
                is_user: false,
            },
        ];
        let g = group_by_parent_chain(&rows);
        // No user root exists, so each row terminates on the cycle close.
        // Both rows are placed in *some* bucket — assertion is that we
        // returned at all (no hang) and didn't drop any rows.
        let total: usize = g.values().map(|v| v.len()).sum();
        assert_eq!(total, 2, "no row dropped under cycle");
    }

    /// Self-loop cycle (`a -> a`). Pathological but cheap to guard.
    #[test]
    fn self_loop_does_not_hang() {
        let rows = vec![TestRow {
            uuid: "a".into(),
            parent_uuid: Some("a".into()),
            is_user: false,
        }];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.values().map(|v| v.len()).sum::<usize>(), 1);
    }

    /// Rows with empty `uuid()` are skipped — they cannot be walked or
    /// bucketed. The Claude reader carries the legacy file-order text
    /// association as the explicit fallback for these rows.
    #[test]
    fn rows_with_empty_uuid_are_skipped() {
        let rows = vec![
            TestRow {
                uuid: String::new(),
                parent_uuid: None,
                is_user: true,
            },
            user("u1", None),
            asst("a1", "u1"),
        ];
        let g = group_by_parent_chain(&rows);
        let total: usize = g.values().map(|v| v.len()).sum();
        assert_eq!(total, 2, "empty-uuid row skipped, other two bucketed");
        assert!(g.contains_key("u1"));
    }

    /// Orphaned chain: row whose parent_uuid points at a UUID not present
    /// in the input. The walk terminates at the deepest reachable row and
    /// uses its UUID as the bucket key — over-grouping fallback per the
    /// issue's "prefer over-grouping to silently dropping rows" guidance.
    #[test]
    fn orphan_chain_uses_deepest_reachable_uuid() {
        let rows = vec![asst("a1", "missing_parent"), asst("a2", "a1")];
        let g = group_by_parent_chain(&rows);
        // a2 walks up to a1, then a1's parent ("missing_parent") is not in
        // the map → bucket key is "a1". a1 also walks once and hits the
        // same dead end → same bucket.
        assert_eq!(g.len(), 1);
        let bucket = g.get("a1").unwrap();
        assert_eq!(bucket.len(), 2);
    }

    /// Multiple disjoint roots (e.g. manual `/resume` produced two
    /// user-prompt chains in the same file) coexist as separate turns.
    #[test]
    fn multiple_disjoint_roots_remain_separate() {
        let rows = vec![
            user("u_a", None),
            asst("a_a", "u_a"),
            user("u_b", None), // disjoint root, parent=null
            asst("a_b", "u_b"),
        ];
        let g = group_by_parent_chain(&rows);
        assert_eq!(g.len(), 2);
        assert!(g.contains_key("u_a"));
        assert!(g.contains_key("u_b"));
    }
}
