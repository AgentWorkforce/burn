//! Per-process content-capture gap tracker — Rust port of the
//! `moduleGapState` machine in `packages/ingest/src/ingest.ts`.
//!
//! A session is "affected" iff a parse pass observed `tool_use` blocks
//! for it in `contentMode === 'full'` mode without any observed
//! `tool_result` [`ContentRecord`] — the load-bearing kind for
//! `burn hotspots`'s tool-call attribution.
//!
//! Tracking is per-process and per-session (not per-call counts), so the
//! set shrinks as later passes pick up the missing tool_result lines.
//! The most common cause of a gap is a session that was still running
//! when ingest observed the assistant tool_use line — the tool_result
//! line gets flushed shortly after and the next pass heals the session.
//! Sessions that were killed mid-call stay flagged permanently, which is
//! the signal we want.
//!
//! Suppression: a warning fires only when the current affected set
//! includes a session that was not present in the last emitted warning.
//! Steady-state or shrinking sets stay silent, but churn that introduces
//! a fresh affected session still re-warns even if the net count stays
//! flat. After the set decays back to zero the suppression marker is
//! cleared so a fresh gap from a future regression triggers a new
//! warning.
//!
//! ## Where the state lives
//!
//! Process-global, behind a `Mutex` in a `OnceLock`, mirroring the TS
//! `moduleGapState` const. The TS version is process-wide because the
//! ingest module is loaded once per Node process; the Rust port keeps
//! the same lifetime so suppression semantics survive across multiple
//! `ingest_*` calls within a single binary invocation (the watch loop
//! fires `ingest_all` repeatedly and relies on this).
//!
//! Tying the state to a `Ledger` handle would force callers to thread
//! the same handle through every ingest call to get suppression — the
//! `burn run` path uses two distinct handles (per-session pre-spawn,
//! sweep post-spawn) and would lose suppression. Process-global keeps
//! the behaviour byte-equivalent to TS without burdening callers.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};

use relayburn_reader::{ContentKind, ContentRecord, ContentStoreMode, TurnRecord};

/// Adapter discriminator for the gap tracker. Mirrors the TS
/// `'claude' | 'codex' | 'opencode'` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterName {
    Claude,
    Codex,
    Opencode,
}

impl AdapterName {
    fn as_str(self) -> &'static str {
        match self {
            AdapterName::Claude => "claude",
            AdapterName::Codex => "codex",
            AdapterName::Opencode => "opencode",
        }
    }
}

type WriterFn = Box<dyn Fn(&str) + Send + Sync>;

struct GapState {
    /// Sessions currently flagged as missing tool_result content, per
    /// adapter.
    affected_sessions: HashMap<AdapterName, HashSet<String>>,
    /// Cumulative orphan tool-call count for each flagged session.
    /// Removed alongside the session when it heals.
    orphan_calls_per_session: HashMap<AdapterName, HashMap<String, u64>>,
    /// Sessions known to have emitted >=1 tool_result content record in
    /// this process. Once a session is here it can never be re-flagged
    /// (capture proved itself for that session at least once). Bounded
    /// by the number of distinct sessionIds the process has touched.
    healed_sessions: HashMap<AdapterName, HashSet<String>>,
    /// Sessions included in the most recent emitted warning, per adapter.
    /// Used to suppress repeats unless a newly affected session appears.
    warned_affected_sessions: HashMap<AdapterName, HashSet<String>>,
    /// Default sink for gap warnings when the caller has not provided an
    /// explicit one. Receives the warning body and is responsible for
    /// whatever framing the sink wants — the default prepends the
    /// warning glyph and a trailing newline. Tests inject a buffer-backed
    /// sink so they can assert on the body without scribbling on stderr.
    write: WriterFn,
}

impl GapState {
    fn new() -> Self {
        Self {
            affected_sessions: HashMap::new(),
            orphan_calls_per_session: HashMap::new(),
            healed_sessions: HashMap::new(),
            warned_affected_sessions: HashMap::new(),
            write: Box::new(default_writer),
        }
    }

    fn clear(&mut self) {
        self.affected_sessions.clear();
        self.orphan_calls_per_session.clear();
        self.healed_sessions.clear();
        self.warned_affected_sessions.clear();
    }
}

fn default_writer(body: &str) {
    // TS default: `process.stderr.write(`⚠ ${body}\n`)`. Match the same
    // glyph + newline framing so a callerless emit lands the same shape
    // on both adapters during the dual-tree window.
    eprintln!("⚠ {body}");
}

fn state_lock() -> MutexGuard<'static, GapState> {
    static STATE: OnceLock<Mutex<GapState>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(GapState::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Test-only: clear per-process gap state. Safe to call from prod code
/// too (it's a no-op when nothing has been observed yet).
///
/// Mirrors the TS `resetIngestGapWarnings()`.
pub fn reset_ingest_gap_warnings() {
    state_lock().clear();
}

/// Test-only: replace the warning sink. Returns the previous sink so
/// callers can restore it. Mirrors the TS `setIngestGapWriter`.
///
/// The returned closure is the previous sink; callers should pass it
/// back to `set_ingest_gap_writer` in a `defer`-equivalent so a panicking
/// test leaves the global state restored on the next test that pulls the
/// guard.
pub fn set_ingest_gap_writer<F>(write: F) -> WriterFn
where
    F: Fn(&str) + Send + Sync + 'static,
{
    let mut state = state_lock();
    std::mem::replace(&mut state.write, Box::new(write))
}

/// Restore a previously captured sink. Convenience for the
/// `let prev = set_ingest_gap_writer(...); ...; restore_ingest_gap_writer(prev);`
/// idiom — equivalent to `setIngestGapWriter(prev)` in TS.
pub fn restore_ingest_gap_writer(write: WriterFn) {
    state_lock().write = write;
}

/// Update the process-wide gap state for one parse pass on one session.
/// Called from each adapter's ingest loop after `parse_*_incremental`
/// returns, regardless of whether the pass produced new turns —
/// `content` arriving without new turns is the heal case (tool_result
/// line landed after its assistant tool_use was already cursored past).
///
/// Mirrors the TS `recordSessionGap`.
pub fn record_session_gap(
    adapter: AdapterName,
    session_id: &str,
    new_tool_calls: u64,
    new_tool_results: u64,
) {
    if session_id.is_empty() {
        return;
    }
    let mut state = state_lock();
    let healed_already = state
        .healed_sessions
        .get(&adapter)
        .map(|s| s.contains(session_id))
        .unwrap_or(false);

    if new_tool_results > 0 {
        // Any tool_result on this session proves capture works for it.
        // Drop orphan detail and immunize against future re-flags in
        // this process, trading per-call precision for stable warning
        // behavior.
        state
            .affected_sessions
            .entry(adapter)
            .or_default()
            .remove(session_id);
        state
            .orphan_calls_per_session
            .entry(adapter)
            .or_default()
            .remove(session_id);
        state
            .healed_sessions
            .entry(adapter)
            .or_default()
            .insert(session_id.to_string());
        return;
    }
    if new_tool_calls == 0 {
        return;
    }
    // Once a session has shown that capture works for it, don't re-flag
    // on a later mid-flight observation; the tool_result will arrive on
    // the next pass and we'd just be flapping.
    if healed_already {
        return;
    }
    state
        .affected_sessions
        .entry(adapter)
        .or_default()
        .insert(session_id.to_string());
    let orphans = state.orphan_calls_per_session.entry(adapter).or_default();
    *orphans.entry(session_id.to_string()).or_insert(0) += new_tool_calls;
}

/// Outcome of [`count_tool_call_gaps`]. Mirrors the TS shape:
/// `{ sessionAffected: boolean; orphanToolCalls: number }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCallGapCounts {
    pub session_affected: bool,
    pub orphan_tool_calls: u64,
}

/// Count tool calls in committed turns while no tool_result
/// [`ContentRecord`] was captured for the session. A session is
/// "affected" iff (a) it produced >=1 turn with >=1 tool call and (b) no
/// tool_result records were captured for it. Per the issue, we ignore
/// the `text` / `thinking` / `tool_use` content kinds because their
/// absence is not load-bearing for `burn hotspots` attribution.
///
/// Mirrors the TS `countToolCallGaps`.
pub fn count_tool_call_gaps(
    turns: &[TurnRecord],
    content: &[ContentRecord],
) -> ToolCallGapCounts {
    let tool_calls_observed: u64 = turns.iter().map(|t| t.tool_calls.len() as u64).sum();
    if tool_calls_observed == 0 {
        return ToolCallGapCounts {
            session_affected: false,
            orphan_tool_calls: 0,
        };
    }
    let tool_results: u64 = content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::ToolResult))
        .count() as u64;
    if tool_results > 0 {
        return ToolCallGapCounts {
            session_affected: false,
            orphan_tool_calls: 0,
        };
    }
    ToolCallGapCounts {
        session_affected: true,
        orphan_tool_calls: tool_calls_observed,
    }
}

/// Sum tool calls across the parsed turns of one batch. Convenience
/// helper for adapters that still want to call into `record_session_gap`
/// directly. Mirrors the TS `countNewToolCalls`.
pub fn count_new_tool_calls(turns: &[TurnRecord]) -> u64 {
    turns.iter().map(|t| t.tool_calls.len() as u64).sum()
}

/// Count `tool_result` [`ContentRecord`] entries in the parsed batch.
/// Mirrors the TS `countNewToolResults`.
pub fn count_new_tool_results(content: &[ContentRecord]) -> u64 {
    content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::ToolResult))
        .count() as u64
}

/// Emit (or suppress) a gap warning for one adapter. Optional `on_warn`
/// receives the warning body when it fires; otherwise the process-global
/// writer takes over.
///
/// Mirrors the TS `emitGapWarning`. Suppression semantics: only fire
/// when the affected set includes at least one session not present in
/// the previous warning. After the affected set decays back to empty,
/// the suppression marker is cleared so a fresh affected session
/// re-emits.
pub fn emit_gap_warning(
    adapter: AdapterName,
    content_mode: ContentStoreMode,
    on_warn: Option<&dyn Fn(&str)>,
) {
    if !matches!(content_mode, ContentStoreMode::Full) {
        return;
    }
    // Snapshot the data we need into owned values before we drop the
    // lock — the warning sink is user-supplied and must not be invoked
    // while we hold the global mutex (a re-entrant call would deadlock).
    let body = {
        let mut state = state_lock();
        let affected_empty = state
            .affected_sessions
            .get(&adapter)
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if affected_empty {
            // Set decayed back to empty — clear the suppression marker
            // so a fresh gap from a future regression triggers a new
            // warning.
            state.warned_affected_sessions.remove(&adapter);
            return;
        }
        let affected = state.affected_sessions.get(&adapter).cloned().unwrap();
        let prior = state.warned_affected_sessions.get(&adapter).cloned();
        let has_fresh_affected_session = match &prior {
            None => true,
            Some(prior_set) => affected.iter().any(|sid| !prior_set.contains(sid)),
        };
        if !has_fresh_affected_session {
            return;
        }
        // Update the suppression marker before we drop the lock so a
        // racing emit on another thread sees the new high-water mark.
        state
            .warned_affected_sessions
            .insert(adapter, affected.clone());
        let total_calls: u64 = state
            .orphan_calls_per_session
            .get(&adapter)
            .map(|m| m.values().copied().sum())
            .unwrap_or(0);
        format_warning_body(adapter, affected.len() as u64, total_calls)
    };
    if let Some(cb) = on_warn {
        cb(&body);
    } else {
        // Re-take the lock to invoke the configured default writer; it's
        // user-supplied too but at least it's the one already wired up.
        let state = state_lock();
        (state.write)(&body);
    }
}

fn format_warning_body(adapter: AdapterName, sessions: u64, total_calls: u64) -> String {
    let session_word = if sessions == 1 { "session" } else { "sessions" };
    let call_word = if total_calls == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    format!(
        "{adapter}: {sessions} {session_word} logged tool calls without any observed tool_result content ({total_calls} {call_word}).\n  Likely cause: still running (result line not yet flushed) or killed mid-call.\n  Counts decay as later ingest passes pick up the result lines; sized hotspots\n  attribution falls back to user-turn block sizes (or even-split) until they heal.",
        adapter = adapter.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    use relayburn_reader::{
        ContentKind, ContentRecord, ContentRole, SourceKind, ToolCall, TurnRecord, Usage,
    };

    // The gap state is process-global, so we serialize the tests that
    // mutate it. A panicking test poisons the lock; recover the inner
    // value so a peer test still runs.
    fn test_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn make_turn(session: &str, message: &str, tool_call_count: usize) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: session.into(),
            session_path: None,
            message_id: message.into(),
            turn_index: 0,
            ts: "2026-04-22T00:00:00.000Z".into(),
            model: "gpt-5".into(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 1,
                output: 1,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: (0..tool_call_count)
                .map(|i| ToolCall {
                    id: format!("{message}-tc-{i}"),
                    name: "exec_command".into(),
                    target: None,
                    args_hash: "h".into(),
                    is_error: None,
                    edit_pre_hash: None,
                    edit_post_hash: None,
                    skill_name: None,
                    replaced_tools: None,
                    collapsed_calls: None,
                })
                .collect(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn make_content(session: &str, message: &str, kind: ContentKind, role: ContentRole) -> ContentRecord {
        ContentRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: session.into(),
            message_id: message.into(),
            ts: "2026-04-22T00:00:00.000Z".into(),
            role,
            kind,
            text: Some("x".into()),
            tool_use: None,
            tool_result: None,
        }
    }

    #[test]
    fn count_tool_call_gaps_flags_orphans() {
        let turns = vec![
            make_turn("sess_test", "m1", 2),
            make_turn("sess_test", "m2", 1),
        ];
        let content = vec![
            make_content("sess_test", "m1", ContentKind::Text, ContentRole::Assistant),
            make_content("sess_test", "m1", ContentKind::ToolUse, ContentRole::Assistant),
        ];
        let r = count_tool_call_gaps(&turns, &content);
        assert!(r.session_affected);
        assert_eq!(r.orphan_tool_calls, 3);
    }

    #[test]
    fn count_tool_call_gaps_chat_only_session() {
        let turns = vec![make_turn("sess_test", "m1", 0)];
        let content = vec![
            make_content("sess_test", "m1", ContentKind::Text, ContentRole::User),
            make_content("sess_test", "m1", ContentKind::Text, ContentRole::Assistant),
        ];
        let r = count_tool_call_gaps(&turns, &content);
        assert!(!r.session_affected);
        assert_eq!(r.orphan_tool_calls, 0);
    }

    #[test]
    fn count_tool_call_gaps_with_tool_result_does_not_flag() {
        let turns = vec![make_turn("sess_test", "m1", 1)];
        let content = vec![
            make_content("sess_test", "m1", ContentKind::ToolUse, ContentRole::Assistant),
            make_content(
                "sess_test",
                "m1",
                ContentKind::ToolResult,
                ContentRole::ToolResult,
            ),
        ];
        let r = count_tool_call_gaps(&turns, &content);
        assert!(!r.session_affected);
    }

    #[test]
    fn warning_fires_once_per_fresh_affected_session() {
        let _g = test_lock();
        reset_ingest_gap_warnings();
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });

        // First pass: a brand-new affected session — fires.
        record_session_gap(AdapterName::Codex, "sess_gap_1", 2, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(captured.lock().unwrap().len(), 1);

        // Steady-state: same affected set — silent.
        record_session_gap(AdapterName::Codex, "sess_gap_1", 0, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "second emit must stay silent for unchanged set"
        );

        // Third pass: still steady — silent.
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(captured.lock().unwrap().len(), 1);

        let body = captured.lock().unwrap()[0].clone();
        assert!(
            body.starts_with("codex: 1 session logged tool calls"),
            "body: {body}"
        );
        assert!(body.contains("(2 tool calls)"));
        assert!(body.contains("still running"));
        assert!(body.contains("Counts decay"));

        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn warning_silent_when_content_mode_is_not_full() {
        let _g = test_lock();
        reset_ingest_gap_warnings();
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });

        record_session_gap(AdapterName::Codex, "sess_gap_1", 2, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::HashOnly, None);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Off, None);
        assert!(captured.lock().unwrap().is_empty());

        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn warning_decays_and_re_emits_after_full_clear() {
        // Mirrors the TS "decays the affected count when a later ingest
        // pass parses tool_result content" case: once the affected set
        // shrinks back to empty the suppression marker clears, so a
        // fresh gap re-ignites the warning.
        let _g = test_lock();
        reset_ingest_gap_warnings();
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });

        // Pass 1: gap session.
        record_session_gap(AdapterName::Codex, "sess_gap_1", 2, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(captured.lock().unwrap().len(), 1);

        // Pass 2: same session heals — affected set is now empty.
        record_session_gap(AdapterName::Codex, "sess_gap_1", 0, 1);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(captured.lock().unwrap().len(), 1, "no new warning after heal");

        // Pass 3: a brand-new gap session re-ignites the warning,
        // proving the suppression marker decayed back to zero (not
        // stuck at 1).
        record_session_gap(AdapterName::Codex, "sess_gap_2", 1, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "fresh gap re-emits after decay"
        );
        let last = captured.lock().unwrap()[1].clone();
        assert!(last.contains("1 session"));

        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn warning_re_emits_when_affected_churn_keeps_count_flat() {
        // Mirrors the TS "re-warns when affected session churn keeps the
        // count flat" case: two sessions affected, one heals, a third
        // becomes affected — net count stays at 2 but a fresh
        // session id appeared, so we must re-emit.
        let _g = test_lock();
        reset_ingest_gap_warnings();
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });

        record_session_gap(AdapterName::Codex, "sess_gap_1", 2, 0);
        record_session_gap(AdapterName::Codex, "sess_gap_2", 1, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(captured.lock().unwrap().len(), 1);
        assert!(captured.lock().unwrap()[0].contains("2 sessions"));
        assert!(captured.lock().unwrap()[0].contains("(3 tool calls)"));

        // Heal the first one; set shrinks to {sess_gap_2}. Suppression
        // marker still {sess_gap_1, sess_gap_2}, no fresh session, silent.
        record_session_gap(AdapterName::Codex, "sess_gap_1", 0, 1);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "shrinking set stays silent"
        );

        // A new gap session pushes the set back to size 2 but with a
        // fresh member (sess_gap_3); fires.
        record_session_gap(AdapterName::Codex, "sess_gap_3", 1, 0);
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "fresh affected session re-emits even at same count"
        );
        let last = captured.lock().unwrap()[1].clone();
        assert!(last.contains("2 sessions"));
        assert!(last.contains("(2 tool calls)"));

        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn record_session_gap_ignores_empty_session_id() {
        let _g = test_lock();
        reset_ingest_gap_warnings();
        record_session_gap(AdapterName::Codex, "", 5, 0);
        // Nothing should land in the affected set.
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert!(captured.lock().unwrap().is_empty());
        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn healed_session_immune_to_re_flag() {
        // Once a session has emitted a tool_result, a later pass that
        // observes a fresh tool_use mid-flight does not re-flag it. The
        // TS code calls this "trading per-call precision for stable
        // warning behaviour" — we mirror it.
        let _g = test_lock();
        reset_ingest_gap_warnings();
        record_session_gap(AdapterName::Codex, "sess_h", 1, 1); // immunize
        record_session_gap(AdapterName::Codex, "sess_h", 5, 0); // would flag
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            captured_clone.lock().unwrap().push(body.to_string());
        });
        emit_gap_warning(AdapterName::Codex, ContentStoreMode::Full, None);
        assert!(
            captured.lock().unwrap().is_empty(),
            "previously healed session must not re-flag"
        );
        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }

    #[test]
    fn on_warn_callback_overrides_default_writer() {
        let _g = test_lock();
        reset_ingest_gap_warnings();
        // Sanity: with a sink installed but a callback supplied, the
        // callback wins and the sink stays untouched.
        let sink_captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink_clone = sink_captured.clone();
        let prev = set_ingest_gap_writer(move |body| {
            sink_clone.lock().unwrap().push(body.to_string());
        });

        record_session_gap(AdapterName::Codex, "sess_cb", 1, 0);
        let cb_captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let cb_clone = cb_captured.clone();
        emit_gap_warning(
            AdapterName::Codex,
            ContentStoreMode::Full,
            Some(&|body| cb_clone.lock().unwrap().push(body.to_string())),
        );
        assert_eq!(cb_captured.lock().unwrap().len(), 1);
        assert!(sink_captured.lock().unwrap().is_empty());

        restore_ingest_gap_writer(prev);
        reset_ingest_gap_warnings();
    }
}
