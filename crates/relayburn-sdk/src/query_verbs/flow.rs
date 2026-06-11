use super::*;

// ---------------------------------------------------------------------------
// Span trees — pure derived view (#430)
// ---------------------------------------------------------------------------
//
// The span tree is the canonical hierarchy for a turn. We derive on
// every call rather than caching: per the issue, "default position:
// always derive; cache only if profiling demands it." If a future
// profile shows hot-path repeat queries against the same turn we'd
// add a memoization layer here, but the surface stays the same.

impl LedgerHandle {
    /// Build the [`TurnSpanTree`] for one turn, identified by its
    /// session + turn id (the `turn_id` matches
    /// [`crate::reader::TurnRecord::message_id`] for Claude; for Codex
    /// / opencode it's the harness's per-turn key, also stored on
    /// `message_id`).
    ///
    /// The tree is built by:
    ///
    /// 1. Looking up the matching [`TurnRecord`] (one query).
    /// 2. Pulling the per-session inferences from the
    ///    [`inferences`](crate::reader::Inference) table — empty on
    ///    a pre-v5 ledger that hasn't been rebuilt, in which case the
    ///    builder falls back to a synthetic single-inference shape.
    /// 3. Pulling the per-session `tool_result_events` and filtering
    ///    to events for the requested turn's `message_id`.
    /// 4. Walking the Claude `subagents/` sidecar tree (lazy — short-
    ///    circuits when the directory is missing) and pairing
    ///    transcripts against the same session's main JSONL. Codex
    ///    turns skip this step entirely (no sidecar concept).
    /// 5. Dispatching to the per-harness builder.
    ///
    /// Returns an error when the requested turn isn't on the ledger.
    pub fn turn_span_tree(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<crate::analyze::span_tree::TurnSpanTree> {
        let trees = self.session_span_trees(session_id)?;
        trees
            .into_iter()
            .find(|t| t.turn_id == turn_id)
            .ok_or_else(|| {
                anyhow::anyhow!("turn not found: session_id={session_id} turn_id={turn_id}")
            })
    }

    /// Build a [`TurnSpanTree`] for every turn in a session, in the
    /// session's stored order.
    ///
    /// Loads each underlying table once and slices per turn — cheaper
    /// than calling [`Self::turn_span_tree`] in a loop when the caller
    /// wants the whole session. Identical contract otherwise: pure
    /// derivation, no caching, no writes.
    pub fn session_span_trees(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::analyze::span_tree::TurnSpanTree>> {
        let session_q = Query {
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let enriched_turns = self.inner.query_turns(&session_q)?;
        let turns: Vec<crate::reader::TurnRecord> =
            enriched_turns.into_iter().map(|e| e.turn).collect();
        if turns.is_empty() {
            return Ok(Vec::new());
        }

        // Source dispatch: every turn in the session shares the same
        // `source` (the ledger never mixes harness rows under one
        // session id), so the first turn's source decides which
        // builder we route to.
        let source = turns[0].source;

        // Bulk-load the per-session sidecar tables.
        //
        // These tables landed in later schema versions (see #434 / #444);
        // a pre-schema ledger reports "no such table" / "no such column".
        // Tolerate that single class of failure so the span-tree builder
        // still works on older snapshots, but propagate every other read
        // error so corrupted ledgers don't silently produce truncated
        // span trees (which would mis-attribute downstream context deltas).
        let inferences = match self.inner.query_inferences(&session_q) {
            Ok(v) => v,
            Err(err) if is_schema_missing(&err) => Vec::new(),
            Err(err) => return Err(err.into()),
        };
        let tool_result_events = match self.inner.query_tool_result_events(&session_q) {
            Ok(v) => v,
            Err(err) if is_schema_missing(&err) => Vec::new(),
            Err(err) => return Err(err.into()),
        };

        // Group sidecars by message_id for fast per-turn slicing.
        let mut infs_by_msg: HashMap<String, Vec<crate::reader::Inference>> = HashMap::new();
        for inf in inferences {
            infs_by_msg
                .entry(inf.turn_id.clone())
                .or_default()
                .push(inf);
        }
        let mut events_by_msg: HashMap<String, Vec<crate::reader::ToolResultEventRecord>> =
            HashMap::new();
        for ev in tool_result_events {
            if let Some(m) = ev.message_id.clone() {
                events_by_msg.entry(m).or_default().push(ev);
            }
        }

        // Subagent transcripts: Claude-only. Even for Claude, the
        // discovery walks a session-scoped directory that's missing
        // for the vast majority of sessions; the lazy stat-check in
        // `discover_subagents` keeps this near-free on miss.
        let subagents = if matches!(source, crate::reader::SourceKind::ClaudeCode) {
            discover_and_pair_subagents(session_id).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Bucket the session-wide subagent slice into per-turn lists so
        // each sidecar lands in exactly one turn. The Claude builder
        // treats `paired_tool_use_id == None` as an unattached child of
        // the turn root, so passing the unfiltered slice into every
        // turn build would duplicate each orphan into every tree.
        //
        // Assignment rule:
        // - **Paired**: assign to the turn whose `tool_calls` carry the
        //   matching `tool_use_id`. (Falls through to the orphan rule
        //   when the pairing references a tool_use we don't have on
        //   ledger — keeps a sidecar reachable rather than dropping it.)
        // - **Orphan**: assign to the latest turn whose `ts <=
        //   subagent_start_ms`; if no turn precedes it (or the sidecar
        //   has no parseable timestamp), assign to the first turn.
        let subagent_buckets = bucket_subagents_per_turn(&turns, &subagents);

        let mut out = Vec::with_capacity(turns.len());
        for (turn_idx, turn) in turns.iter().enumerate() {
            let infs_for_turn = infs_by_msg
                .get(&turn.message_id)
                .cloned()
                .unwrap_or_default();
            let events_for_turn = events_by_msg
                .get(&turn.message_id)
                .cloned()
                .unwrap_or_default();
            let subagents_for_turn: Vec<crate::reader::SubagentTranscript> = subagent_buckets
                .get(&turn_idx)
                .map(|idxs| idxs.iter().map(|i| subagents[*i].clone()).collect())
                .unwrap_or_default();
            let tree = match source {
                crate::reader::SourceKind::ClaudeCode => {
                    crate::reader::build_claude_span_tree(crate::reader::ClaudeSpanTreeInputs {
                        turn,
                        tool_result_events: &events_for_turn,
                        inferences: &infs_for_turn,
                        subagents: &subagents_for_turn,
                    })
                }
                _ => crate::reader::build_codex_span_tree(crate::reader::CodexSpanTreeInputs {
                    turn,
                    tool_result_events: &events_for_turn,
                    inferences: &infs_for_turn,
                }),
            };
            out.push(tree);
        }
        Ok(out)
    }

    /// Build the per-session inference-flow DAG (issue #431).
    ///
    /// Convenience wrapper: pulls the session's [`TurnSpanTree`]s via
    /// [`Self::session_span_trees`] and projects them through
    /// [`crate::analyze::flow_graph_from_trees`]. Pure read; no DB
    /// writes, no caching. Honors [`crate::analyze::FlowOpts::max_turns`].
    pub fn flow_graph(
        &self,
        session_id: &str,
        opts: crate::analyze::FlowOpts,
    ) -> Result<crate::analyze::FlowGraph> {
        let trees = self.session_span_trees(session_id)?;
        Ok(crate::analyze::flow_graph_from_trees(
            session_id, &trees, opts,
        ))
    }
}

/// Bucket subagent transcripts into per-turn lists for the span-tree
/// builder. Returns `turn_index -> Vec<subagent_index>` keyed against
/// the slice positions of `turns` and `subagents` so the caller can
/// resolve back into the source vectors without re-borrowing.
///
/// Each subagent lands in **exactly one** turn. See [`LedgerHandle::session_span_trees`]
/// for the rule.
pub(crate) fn bucket_subagents_per_turn(
    turns: &[crate::reader::TurnRecord],
    subagents: &[crate::reader::SubagentTranscript],
) -> HashMap<usize, Vec<usize>> {
    let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
    if turns.is_empty() || subagents.is_empty() {
        return out;
    }

    // Map tool_use_id -> turn_index for paired-sidecar routing.
    let mut tool_use_to_turn: HashMap<&str, usize> = HashMap::new();
    for (idx, turn) in turns.iter().enumerate() {
        for tc in &turn.tool_calls {
            tool_use_to_turn.insert(tc.id.as_str(), idx);
        }
    }

    // Pre-compute turn start_ms (parsed once) for the orphan binary
    // search. Cheap — one parse per turn.
    let turn_starts: Vec<i64> = turns
        .iter()
        .map(|t| parse_iso_ms_compat(&t.ts).unwrap_or(0))
        .collect();

    for (sa_idx, sa) in subagents.iter().enumerate() {
        let mut assigned: Option<usize> = None;
        if let Some(tu) = sa.paired_tool_use_id.as_deref() {
            if !tu.is_empty() {
                if let Some(idx) = tool_use_to_turn.get(tu) {
                    assigned = Some(*idx);
                }
            }
        }
        if assigned.is_none() {
            // Orphan: pick the latest turn whose start_ms <= subagent
            // start_ms. The subagent start is the earliest `timestamp`
            // field on its raw records; fall back to the first turn
            // when the sidecar carries no parseable timestamp.
            let sa_start_ms = first_record_ts_ms(&sa.records);
            assigned = Some(match sa_start_ms {
                Some(sa_ms) => turn_starts
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, ts)| **ts <= sa_ms)
                    .map(|(i, _)| i)
                    .unwrap_or(0),
                None => 0,
            });
        }
        if let Some(idx) = assigned {
            out.entry(idx).or_default().push(sa_idx);
        }
    }
    out
}

/// Extract the earliest `timestamp` field from a subagent's raw JSONL
/// records, returning epoch-millis. Used by the orphan-assignment rule
/// to place sidecars under the latest preceding turn.
fn first_record_ts_ms(records: &[serde_json::Value]) -> Option<i64> {
    let mut earliest: Option<i64> = None;
    for rec in records {
        let ts_str = rec
            .get("timestamp")
            .and_then(|v| v.as_str())
            .or_else(|| rec.get("ts").and_then(|v| v.as_str()));
        if let Some(s) = ts_str {
            if let Some(ms) = parse_iso_ms_compat(s) {
                earliest = Some(match earliest {
                    Some(e) => e.min(ms),
                    None => ms,
                });
            }
        }
    }
    earliest
}

/// ISO-8601 parser thin wrapper. Reuses the shared `crate::util::time`
/// helper so all four ex-copies stay in sync.
fn parse_iso_ms_compat(s: &str) -> Option<i64> {
    crate::util::time::parse_iso_ms(s)
}

/// Resolve the Claude projects root and discover + pair subagent
/// sidecars for `session_id`. Returns an empty `Vec` when:
///
/// - The projects root doesn't exist (no Claude on this machine).
/// - The session's sidecar directory doesn't exist (most sessions).
/// - The parent JSONL is missing (every sidecar surfaces as orphan).
///
/// We resolve `BURN_CLAUDE_PROJECTS_DIR` first to mirror what the
/// summary path does (so the test suite can pin a sandbox); otherwise
/// fall back to `$HOME/.claude/projects`.
fn discover_and_pair_subagents(session_id: &str) -> Result<Vec<crate::reader::SubagentTranscript>> {
    let root = if let Some(p) = std::env::var_os("BURN_CLAUDE_PROJECTS_DIR") {
        std::path::PathBuf::from(p)
    } else {
        // `HOME` is unset on stock Windows shells (`USERPROFILE` carries
        // the user home there). Fall back to it before degenerating to
        // `.` so a Claude Code install on Windows still resolves to
        // `%USERPROFILE%\.claude\projects` without the caller having
        // to set `BURN_CLAUDE_PROJECTS_DIR` explicitly.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        home.join(".claude").join("projects")
    };
    if !root.exists() {
        return Ok(Vec::new());
    }
    // We don't know which project subdir the session lives under
    // without the ledger storing it explicitly. Walk one level deep
    // looking for a project that has a matching session dir.
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        let candidate = project_dir.join(session_id).join("subagents");
        if !candidate.exists() {
            continue;
        }
        let subs = crate::reader::discover_subagents(&project_dir, session_id);
        if subs.is_empty() {
            continue;
        }
        let parent_jsonl = project_dir.join(format!("{session_id}.jsonl"));
        let parent_records = read_jsonl_values(&parent_jsonl);
        return Ok(crate::reader::pair_to_main(&parent_records, subs));
    }
    Ok(Vec::new())
}

/// Load a JSONL file into a `Vec<serde_json::Value>`. Returns empty on
/// any I/O / parse failure — `pair_subagents_to_main` treats every
/// sidecar as orphan in that case, which is the right fallback when
/// the parent transcript is missing or corrupt.
fn read_jsonl_values(path: &Path) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    reader
        .lines()
        .filter_map(|line| {
            let l = line.ok()?;
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str::<serde_json::Value>(t).ok()
            }
        })
        .collect()
}

/// Free-function form of [`LedgerHandle::turn_span_tree`].
pub fn turn_span_tree(
    session_id: &str,
    turn_id: &str,
    ledger_home: Option<PathBuf>,
) -> Result<crate::analyze::span_tree::TurnSpanTree> {
    let handle = open_with(ledger_home.as_deref())?;
    handle.turn_span_tree(session_id, turn_id)
}

/// Free-function form of [`LedgerHandle::session_span_trees`].
pub fn session_span_trees(
    session_id: &str,
    ledger_home: Option<PathBuf>,
) -> Result<Vec<crate::analyze::span_tree::TurnSpanTree>> {
    let handle = open_with(ledger_home.as_deref())?;
    handle.session_span_trees(session_id)
}

/// Free-function form of [`LedgerHandle::flow_graph`].
pub fn flow_graph(
    session_id: &str,
    opts: crate::analyze::FlowOpts,
    ledger_home: Option<PathBuf>,
) -> Result<crate::analyze::FlowGraph> {
    let handle = open_with(ledger_home.as_deref())?;
    handle.flow_graph(session_id, opts)
}

// ---------------------------------------------------------------------------
// Context delta — per-inference attribution of context-window growth (#432)
// ---------------------------------------------------------------------------
//
// Pure derivation over `session_span_trees`. The `ledger_home` plumbing is
// the only I/O; the math lives in `analyze::context_delta::deltas_for_session`.

/// Return `true` when `err` looks like a pre-schema "table / column missing"
/// SQLite failure. Used to distinguish a tolerable "this ledger predates the
/// inferences / tool_result_events tables" miss from a real ledger-read
/// failure that should propagate to the caller.
fn is_schema_missing(err: &crate::ledger::LedgerError) -> bool {
    let crate::ledger::LedgerError::Sqlite(rusqlite::Error::SqliteFailure(_, Some(msg))) = err
    else {
        return false;
    };
    msg.contains("no such table") || msg.contains("no such column")
}

/// Convert a relative `Duration` window into a canonical
/// `now - duration` ISO-8601 timestamp suitable for a [`Query::since`]
/// filter. Centralized so the deltas seed-query mirrors the same
/// `format_iso_z_ms` shape the rest of the SDK emits.
pub(crate) fn duration_to_since_iso(d: std::time::Duration) -> String {
    let now = system_now_secs();
    let when = now.saturating_sub(d.as_secs()) as i64;
    format_iso_z_ms(when, 0)
}

/// Lex key for sorting cross-session [`ContextDelta`] rows by owner_rail
/// when other tie-breakers are equal. Mirrors the per-session helper in
/// `analyze::context_delta`.
fn owner_rail_str(rail: &crate::analyze::context_delta::OwnerRail) -> (&str, &str) {
    match rail {
        crate::analyze::context_delta::OwnerRail::Main => ("main", ""),
        crate::analyze::context_delta::OwnerRail::Subagent { agent_id } => {
            ("subagent", agent_id.as_str())
        }
    }
}

impl LedgerHandle {
    /// Per-inference context-window deltas.
    ///
    /// Walks each session's [`TurnSpanTree`] timeline, pairs same-rail
    /// `Inference` spans, and attributes the delta in `context_tokens =
    /// input + cache_read + cache_write` to the intervening
    /// [`InterveningStep`]s. See the module-level docs of
    /// [`crate::analyze::context_delta`] for the algorithm and the
    /// decision rationale (cost rate, compaction handling, rail
    /// isolation).
    ///
    /// When [`ContextDeltaOpts::session`] is `Some`, only that session is
    /// scanned. When `None`, every session in the ledger that has activity
    /// inside the [`ContextDeltaOpts::since`] window contributes — sessions
    /// whose latest activity falls outside the window are skipped before any
    /// span trees get loaded. The same window is then applied to the
    /// returned [`Vec<ContextDelta>`] cap.
    pub fn context_delta(
        &self,
        opts: crate::analyze::context_delta::ContextDeltaOpts,
    ) -> Result<Vec<crate::analyze::context_delta::ContextDelta>> {
        let pricing = load_pricing(None);

        // Build the seed `since` filter from `opts.since`. We always have a
        // sensible `effective_since()` default, but only apply it when the
        // caller actually passed a value — when `None`, scan every session.
        // (Honoring the default would change historic behavior for callers
        // that relied on "no since = all time".)
        let seed_since: Option<String> = opts.since.map(duration_to_since_iso);
        let session_query = Query {
            since: seed_since.clone(),
            ..Default::default()
        };

        let session_ids: Vec<String> = match opts.session.clone() {
            Some(id) => vec![id],
            None => {
                // Enumerate sessions that have activity inside the
                // `since` window. Walking only the matching `turns`
                // rows keeps this cheap on large ledgers — we never
                // load span trees for sessions that already missed
                // the filter.
                let mut ids: BTreeSet<String> = BTreeSet::new();
                let all = self.inner.query_turns(&session_query)?;
                for enriched in all {
                    ids.insert(enriched.turn.session_id);
                }
                ids.into_iter().collect()
            }
        };

        let mut out: Vec<crate::analyze::context_delta::ContextDelta> = Vec::new();
        for session_id in session_ids {
            let trees = self.session_span_trees(&session_id)?;
            if trees.is_empty() {
                continue;
            }
            let compactions = self.inner.query_compactions(&Query {
                session_id: Some(session_id.clone()),
                ..Default::default()
            })?;
            let per_session = crate::analyze::context_delta::deltas_for_session(
                &trees,
                &compactions,
                &pricing,
                &opts,
            );
            out.extend(per_session);
        }

        // Cross-session sort + top cap. `deltas_for_session` already
        // sorted within a single session; re-sort here so multi-session
        // calls return a single coherent top-N list. Tie chain includes
        // `owner_rail` so subagent-vs-main ties stay stable across
        // HashMap iteration order from the per-session pass.
        out.sort_by(|a, b| {
            b.delta_tokens
                .cmp(&a.delta_tokens)
                .then_with(|| a.session_id.cmp(&b.session_id))
                .then_with(|| a.turn_id.cmp(&b.turn_id))
                .then_with(|| a.inference_idx.cmp(&b.inference_idx))
                .then_with(|| owner_rail_str(&a.owner_rail).cmp(&owner_rail_str(&b.owner_rail)))
        });
        let top = opts.effective_top() as usize;
        if out.len() > top {
            out.truncate(top);
        }
        Ok(out)
    }
}

/// Free-function form of [`LedgerHandle::context_delta`].
pub fn context_delta(
    opts: crate::analyze::context_delta::ContextDeltaOpts,
    ledger_home: Option<PathBuf>,
) -> Result<Vec<crate::analyze::context_delta::ContextDelta>> {
    let handle = open_with(ledger_home.as_deref())?;
    handle.context_delta(opts)
}
