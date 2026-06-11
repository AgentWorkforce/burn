use super::*;

// ---------------------------------------------------------------------------
// state_status — derived-state report for `burn state status`
// ---------------------------------------------------------------------------

/// Per-table row counts in `burn.sqlite`. First-seen order of fields matches
/// the human-render layout the CLI emits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnDbRowCounts {
    pub turns: u64,
    pub user_turns: u64,
    pub compactions: u64,
    pub relationships: u64,
    pub tool_result_events: u64,
    /// v5-added per-API-call aggregate (issue #434). Empty until at
    /// least one `burn ingest` runs against a Claude session; pre-v5
    /// ledgers stay zero until `burn state rebuild`.
    pub inferences: u64,
    pub sessions: u64,
    pub stamps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnDbStatus {
    pub path: String,
    pub exists: bool,
    pub rows: BurnDbRowCounts,
    /// Sum of the per-table row counts in `rows`. Named `tracked_rows`
    /// (not `total_rows`) because `burn.sqlite` also holds the singleton
    /// `archive_state` metadata row, which is reported separately under
    /// `archive` and is deliberately excluded from this total. Renaming
    /// keeps the field name honest about its scope.
    pub tracked_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentDbStatus {
    pub path: String,
    pub exists: bool,
    pub rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStateStatus {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_built_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rebuild_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateConfigSummary {
    pub store: String,
    /// Numeric retention window in days, or `null` when retention is `forever`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<f64>,
    /// `true` iff retention is configured as `forever`.
    pub retention_forever: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateStatus {
    pub home: String,
    pub burn: BurnDbStatus,
    pub content: ContentDbStatus,
    pub archive: ArchiveStateStatus,
    pub config: StateConfigSummary,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateStatusOptions {
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Compose a [`StateStatus`] report describing the on-disk layout of
    /// the open ledger: file paths/sizes for the two SQLite databases,
    /// per-table row counts in `burn.sqlite`, the row count in
    /// `content.sqlite`, the `archive_state` schema/last-built/last-rebuild
    /// fields, and the resolved [`crate::BurnConfig`].
    pub fn state_status(&self) -> Result<StateStatus> {
        let burn_path = self.inner.burn_path().to_path_buf();
        let content_path = self.inner.content_path().to_path_buf();

        // We deliberately don't report file sizes here. WAL checkpointing
        // grows the SQLite files in non-deterministic increments after
        // the first write transaction, so a size readout would drift
        // across runs even on a logically-empty ledger. Callers that
        // need disk-usage info should `du` the files directly.
        let burn_exists = fs::metadata(&burn_path).is_ok();
        let content_exists = fs::metadata(&content_path).is_ok();

        let rows = BurnDbRowCounts {
            turns: self.inner.count_table("turns")? as u64,
            user_turns: self.inner.count_table("user_turns")? as u64,
            compactions: self.inner.count_table("compactions")? as u64,
            relationships: self.inner.count_table("relationships")? as u64,
            tool_result_events: self.inner.count_table("tool_result_events")? as u64,
            inferences: self.inner.count_table("inferences")? as u64,
            sessions: self.inner.count_table("sessions")? as u64,
            stamps: self.inner.count_table("stamps")? as u64,
        };
        let tracked_rows = rows.turns
            + rows.user_turns
            + rows.compactions
            + rows.relationships
            + rows.tool_result_events
            + rows.inferences
            + rows.sessions
            + rows.stamps;

        let archive = read_archive_state(&self.inner)?;
        // Plumb the *active* ledger home into config loading so that a
        // `--ledger-path` override doesn't mix one home's databases with
        // another home's retention settings. We derive the home from the
        // already-resolved burn.sqlite path (its parent directory) — this
        // is the same value reported in `StateStatus::home`, so there's
        // no risk of the config and DB views diverging.
        let active_home: Option<&Path> = burn_path.parent();
        let config = resolve_config_summary(active_home)?;

        // Render paths through the home directory if both share a common
        // ancestor. The CLI normalizer rewrites the absolute fixture path
        // to ${RELAYBURN_HOME}; keep them as plain strings here so the
        // structured output is faithful and the presenter does any
        // home-relative rewriting.
        let home = burn_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(StateStatus {
            home,
            burn: BurnDbStatus {
                path: burn_path.to_string_lossy().into_owned(),
                exists: burn_exists,
                rows,
                tracked_rows,
            },
            content: ContentDbStatus {
                path: content_path.to_string_lossy().into_owned(),
                exists: content_exists,
                rows: self.inner.count_content()? as u64,
            },
            archive,
            config,
        })
    }
}

/// Free-function form of [`LedgerHandle::state_status`] — opens a ledger
/// from `opts.ledger_home` (or the env-var default) and returns the status.
pub fn state_status(opts: StateStatusOptions) -> Result<StateStatus> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.state_status()
}

fn read_archive_state(ledger: &crate::RawLedger) -> Result<ArchiveStateStatus> {
    // The archive_state row is created by `Ledger::open` (DDL inserts id=1
    // ON CONFLICT DO NOTHING), so this query is reliable. Reach through
    // the public `count_table` surface for schema_version by querying via
    // a small helper; rusqlite is exposed via the raw `Ledger` so we use
    // its connection directly through a query method.
    let json: String = ledger.read_archive_state_json()?;
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    struct Raw {
        schema_version: u32,
        #[serde(default)]
        last_built_at: Option<String>,
        #[serde(default)]
        last_rebuild_at: Option<String>,
    }
    let raw: Raw = serde_json::from_str(&json).map_err(|e| anyhow::anyhow!(e))?;
    Ok(ArchiveStateStatus {
        schema_version: raw.schema_version,
        last_built_at: raw.last_built_at,
        last_rebuild_at: raw.last_rebuild_at,
    })
}

/// Resolve the configured `store` + `retention` into a status-friendly
/// summary, scoped to a specific ledger home when supplied. Surfaces
/// errors from `load_config_with_home` instead of swallowing them with
/// `unwrap_or_default()` — under `--ledger-path foo state status` the
/// caller has explicit intent to inspect derived state, and silently
/// reporting default retention/store when the file (or the home itself)
/// can't be read would make the status report misleading.
///
/// `home: None` retains the env-var-driven default home (matches the
/// behaviour ingest already has via bare `load_config()`).
fn resolve_config_summary(home: Option<&Path>) -> Result<StateConfigSummary> {
    let cfg = crate::ledger::load_config_with_home(home)?;
    let store = match cfg.content.store {
        crate::reader::ContentStoreMode::Full => "full",
        crate::reader::ContentStoreMode::HashOnly => "hash-only",
        crate::reader::ContentStoreMode::Off => "off",
    }
    .to_string();
    Ok(match cfg.content.retention_days {
        crate::ledger::Retention::Forever => StateConfigSummary {
            store,
            retention_days: None,
            retention_forever: true,
        },
        crate::ledger::Retention::Days(d) => StateConfigSummary {
            store,
            retention_days: Some(d),
            retention_forever: false,
        },
    })
}

// ---------------------------------------------------------------------------
// fingerprint — cheap polling primitive (count:max_ts:total_bytes)
//
// A stable `{count}:{maxMtime}:{totalSize}` triple lets MCP clients and
// dashboards detect "did anything change" without re-querying or
// re-ingesting.
//
// The actual SQL lives on `RawLedger::ledger_fingerprint`; the verb here
// is the wire-shaped wrapper (a typed `Fingerprint(String)` newtype
// plus the public `FingerprintScope`) and the free-function form that
// opens its own handle.
// ---------------------------------------------------------------------------

/// Scope filter for [`LedgerHandle::fingerprint`] / [`fingerprint`]. One
/// of `AllSessions` / `Session(id)` / `Project(path)`. `Project` takes a
/// `PathBuf` (mirroring the other path-typed SDK fields) but is matched
/// against both the `project` and `project_key` columns on `turns`, so
/// callers can pass either the human path or its normalized key.
#[derive(Debug, Clone)]
pub enum FingerprintScope {
    AllSessions,
    Session(String),
    Project(PathBuf),
}

impl FingerprintScope {
    fn to_ledger(&self) -> crate::ledger::LedgerFingerprintScope {
        match self {
            Self::AllSessions => crate::ledger::LedgerFingerprintScope::AllSessions,
            Self::Session(s) => crate::ledger::LedgerFingerprintScope::Session(s.clone()),
            Self::Project(p) => {
                crate::ledger::LedgerFingerprintScope::Project(p.to_string_lossy().into_owned())
            }
        }
    }
}

/// `{count}:{max_ts}:{total_bytes}` triple. String wrapper so callers
/// compare with bare equality (`a == b`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(pub String);

impl Fingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintOptions {
    /// Restrict to a single `session_id`.
    pub session: Option<String>,
    /// Restrict to a single `project` (matched against `project` or
    /// `project_key`).
    pub project: Option<PathBuf>,
    pub ledger_home: Option<PathBuf>,
}

impl FingerprintOptions {
    pub(crate) fn scope(&self) -> Result<FingerprintScope> {
        match (self.session.as_deref(), self.project.as_ref()) {
            (Some(_), Some(_)) => {
                anyhow::bail!("fingerprint: pass at most one of `session` or `project`")
            }
            (Some(s), None) => Ok(FingerprintScope::Session(s.to_string())),
            (None, Some(p)) => Ok(FingerprintScope::Project(p.clone())),
            (None, None) => Ok(FingerprintScope::AllSessions),
        }
    }
}

impl LedgerHandle {
    /// Compute the ledger fingerprint for `scope`. The result is a
    /// `{count}:{max_ts}:{total_bytes}` string suitable for equality
    /// checks against a previously-stored value — the canonical "did
    /// anything change since I last looked" gate. Microseconds-level
    /// on a 100k-row ledger (single SQL roundtrip).
    pub fn fingerprint(&self, scope: FingerprintScope) -> Result<Fingerprint> {
        let raw = self.inner.ledger_fingerprint(&scope.to_ledger())?;
        Ok(Fingerprint(raw))
    }
}

/// Free-function form of [`LedgerHandle::fingerprint`] — opens its own
/// ledger handle from `opts.ledger_home`.
pub fn fingerprint(opts: FingerprintOptions) -> Result<Fingerprint> {
    let scope = opts.scope()?;
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.fingerprint(scope)
}
