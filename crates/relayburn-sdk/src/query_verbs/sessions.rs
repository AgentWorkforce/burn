use super::*;

// ---------------------------------------------------------------------------
// session_cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCostOptions {
    pub session: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCostResult {
    pub session_id: Option<String>,
    #[serde(rename = "totalUSD")]
    pub total_usd: f64,
    pub total_tokens: u64,
    pub turn_count: u64,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl LedgerHandle {
    pub fn session_cost(&self, opts: SessionCostOptions) -> Result<SessionCostResult> {
        let Some(session_id) = opts.session.clone() else {
            return Ok(SessionCostResult {
                session_id: None,
                total_usd: 0.0,
                total_tokens: 0,
                turn_count: 0,
                models: Vec::new(),
                note: Some("no session id provided".to_string()),
            });
        };
        let q = Query::for_session(&session_id);
        let turns = collect_turns(self, &q)?;
        if turns.is_empty() {
            return Ok(SessionCostResult {
                session_id: Some(session_id),
                total_usd: 0.0,
                total_tokens: 0,
                turn_count: 0,
                models: Vec::new(),
                note: Some("no turns recorded for this session yet".to_string()),
            });
        }
        let pricing = load_pricing(None);
        let mut models = std::collections::BTreeSet::new();
        let mut total_tokens: u64 = 0;
        let mut costs = Vec::with_capacity(turns.len());
        for t in &turns {
            models.insert(t.model.clone());
            let u = &t.usage;
            total_tokens += u.input
                + u.output
                + u.reasoning
                + u.cache_read
                + u.cache_create_5m
                + u.cache_create_1h;
            if let Some(c) = cost_for_turn(t, &pricing) {
                costs.push(c);
            }
        }
        let total = sum_costs(costs.iter());
        let total_usd = (total.total * 1_000_000.0).round() / 1_000_000.0;
        Ok(SessionCostResult {
            session_id: Some(session_id),
            total_usd,
            total_tokens,
            turn_count: turns.len() as u64,
            models: models.into_iter().collect(),
            note: None,
        })
    }
}

pub fn session_cost(opts: SessionCostOptions) -> Result<SessionCostResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.session_cost(SessionCostOptions {
        ledger_home: None,
        ..opts
    })
}

// ---------------------------------------------------------------------------
// inferences — per-API-call rollup (#434)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferencesOptions {
    /// Restrict to a single session. Required for the typical "show me
    /// the API-call timeline of session X" use case; cross-session
    /// fan-outs should call without it.
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Read per-API-call inferences (issue #434). One row per
    /// `(source, session_id, request_id)` triple — the unit a downstream
    /// "how many API calls" surface should consume rather than counting
    /// raw assistant turns (a multi-block Claude inference produces one
    /// `TurnRecord` already, but the inference key is the durable
    /// per-API-call identity even when the harness changes how it
    /// chunks rows).
    pub fn inferences(&self, opts: InferencesOptions) -> Result<Vec<crate::reader::Inference>> {
        let q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
            None,
        )?;
        Ok(self.inner.query_inferences(&q)?)
    }
}

pub fn inferences(opts: InferencesOptions) -> Result<Vec<crate::reader::Inference>> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.inferences(InferencesOptions {
        ledger_home: None,
        ..opts
    })
}

// ---------------------------------------------------------------------------
// sessions_list
// ---------------------------------------------------------------------------

/// Default row cap when `SessionsListOptions::limit` is `None`. Picked to
/// match the "find a session to review" scroll budget — a tighter cap than
/// the typical agent's recent-session count, with `--limit` for callers
/// that want more.
pub const SESSIONS_LIST_DEFAULT_LIMIT: u64 = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsListOptions {
    /// Slice the ledger to events at or after this point. Same parser as
    /// every other verb's `since` (relative `24h`/`7d`/`4w`/`2m` or ISO).
    pub since: Option<String>,
    /// Restrict to a single project (matches `project` or `projectKey`).
    pub project: Option<String>,
    /// Case-insensitive substring filter against `session_id` and the
    /// resolved project label. Kept simple — FTS5 is not consulted here.
    pub grep: Option<String>,
    /// Row cap. Defaults to [`SESSIONS_LIST_DEFAULT_LIMIT`] when `None`.
    pub limit: Option<u64>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionListEntry {
    /// Full session id. Renderers should preserve this exactly.
    pub session_id: String,
    /// Project label (`project` if present, falling back to `projectKey`).
    /// `None` when neither field was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// ISO timestamp of the earliest turn within the filter window.
    pub started_at: String,
    /// ISO timestamp of the latest turn within the filter window.
    pub last_seen: String,
    pub turn_count: u64,
    #[serde(rename = "totalCostUSD")]
    pub total_cost_usd: f64,
    /// Distinct models observed in the session, sorted lexicographically.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionsListResult {
    /// Sessions ordered by `last_seen` descending — most-recent first.
    pub sessions: Vec<SessionListEntry>,
    /// Effective row cap used for the response (the `limit` flag, defaulted).
    pub limit: u64,
    /// `true` when the underlying turn scan was truncated by `limit`. Lets
    /// callers tell "no more sessions" apart from "more exist; widen the
    /// cap to see them".
    pub truncated: bool,
}

impl LedgerHandle {
    /// Enumerate sessions in the ledger most-recent first. Derived from the
    /// `turns` table rather than `sessions` because the latter may be empty
    /// in older ledgers (the canonical source of truth is the per-turn rows
    /// every other read verb already trusts).
    pub fn sessions_list(&self, opts: SessionsListOptions) -> Result<SessionsListResult> {
        let limit = opts.limit.unwrap_or(SESSIONS_LIST_DEFAULT_LIMIT);
        let q = build_query(None, opts.project.as_deref(), opts.since.as_deref(), None)?;
        let turns = collect_turns(self, &q)?;

        let pricing = load_pricing(None);
        // Aggregate per-session in a single pass over the turn stream.
        let mut acc: BTreeMap<String, SessionAccumulator> = BTreeMap::new();
        for turn in &turns {
            let entry = acc.entry(turn.session_id.clone()).or_default();
            entry.add_turn(turn, &pricing);
        }

        let needle = opts.grep.as_ref().map(|s| s.to_lowercase());
        let mut entries: Vec<SessionListEntry> = acc
            .into_iter()
            .map(|(session_id, acc)| acc.into_entry(session_id))
            .filter(|entry| match needle.as_deref() {
                None => true,
                Some(needle) => {
                    let project_match = entry
                        .project
                        .as_deref()
                        .map(|p| p.to_lowercase().contains(needle))
                        .unwrap_or(false);
                    project_match || entry.session_id.to_lowercase().contains(needle)
                }
            })
            .collect();

        // Most-recent first; tie-break on session_id for stable ordering when
        // two sessions share a last_seen ts (mostly tests, but worth pinning).
        entries.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        let truncated = entries.len() as u64 > limit;
        entries.truncate(limit as usize);

        Ok(SessionsListResult {
            sessions: entries,
            limit,
            truncated,
        })
    }
}

pub fn sessions_list(opts: SessionsListOptions) -> Result<SessionsListResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.sessions_list(SessionsListOptions {
        ledger_home: None,
        ..opts
    })
}

#[derive(Default)]
struct SessionAccumulator {
    started_at: Option<String>,
    last_seen: Option<String>,
    turn_count: u64,
    cost_total: f64,
    project: Option<String>,
    models: BTreeSet<String>,
}

impl SessionAccumulator {
    fn add_turn(&mut self, turn: &TurnRecord, pricing: &PricingTable) {
        self.turn_count += 1;
        match self.started_at.as_ref() {
            Some(cur) if cur.as_str() <= turn.ts.as_str() => {}
            _ => self.started_at = Some(turn.ts.clone()),
        }
        match self.last_seen.as_ref() {
            Some(cur) if cur.as_str() >= turn.ts.as_str() => {}
            _ => self.last_seen = Some(turn.ts.clone()),
        }
        if self.project.is_none() {
            // Mirror the resolution `Query.project` filters on so the rendered
            // column matches the value users would pass to `--project`.
            self.project = turn.project.clone().or_else(|| turn.project_key.clone());
        }
        self.models.insert(turn.model.clone());
        if let Some(c) = cost_for_turn(turn, pricing) {
            self.cost_total += c.total;
        }
    }

    fn into_entry(self, session_id: String) -> SessionListEntry {
        SessionListEntry {
            session_id,
            project: self.project,
            started_at: self.started_at.unwrap_or_default(),
            last_seen: self.last_seen.unwrap_or_default(),
            turn_count: self.turn_count,
            // Round to 6 decimals — same precision contract `session_cost`
            // uses, so the two surfaces are byte-comparable.
            total_cost_usd: (self.cost_total * 1_000_000.0).round() / 1_000_000.0,
            models: self.models.into_iter().collect(),
        }
    }
}
