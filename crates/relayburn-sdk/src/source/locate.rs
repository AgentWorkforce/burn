//! Resolve one harness session into relayhistory evidence.
//!
//! An id is looked up in a relayhistory store: hydrated when catalogued,
//! otherwise discovered for that harness alone and then hydrated. A path is
//! read through a throwaway store, so analyzing a transcript persists
//! nothing; see [`super::stage`] for how each harness's artifact is laid out
//! for relayhistory.

use std::path::PathBuf;

use ai_hist::{
    DiscoveryOptions, HydrateOptions, HydrateStatus, ProviderRoots, SessionEvidence, SessionQuery,
    SessionRef, SessionStore, Source, StoreOptions,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::delegated::delegated_children;
use super::stage::{stage_path, Staged};
use crate::reader::Harness;

/// One harness session, named by id or by the artifact on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "camelCase")]
pub enum SessionLocator {
    /// A session in a relayhistory store, discovered on demand from the
    /// store's provider roots.
    #[serde(rename_all = "camelCase")]
    Id {
        harness: Harness,
        session_id: String,
    },
    /// A session artifact: a Claude Code or Codex transcript JSONL, or an
    /// OpenCode session metadata file inside its `storage/` tree
    /// (`storage/session/<scope>/<id>.json`).
    #[serde(rename_all = "camelCase")]
    Path { harness: Harness, path: PathBuf },
}

/// The relayhistory store that resolves [`SessionLocator::Id`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStoreOptions {
    /// The ai-hist database. Defaults to `$AI_HIST_DB`, then the XDG data
    /// path (`<home>/.local/share/ai-hist/ai-history.db` when `home` is set).
    pub db_path: Option<PathBuf>,
    /// Provider home whose harness stores are read (`<home>/.claude`,
    /// `<home>/.codex`, `<home>/.local/share/opencode`), instead of `$HOME`
    /// with the `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `OPENCODE_DB` overrides.
    pub home: Option<PathBuf>,
}

/// One session's evidence plus where it was read from.
pub(crate) struct LoadedSession {
    pub evidence: SessionEvidence,
    /// Evidence of the Claude subagents the session delegated work to,
    /// nested ones included; see [`super::delegated`].
    pub children: Vec<SessionEvidence>,
    /// Provider roots of the harness install the session came from. `None`
    /// when the artifact was staged outside any harness install.
    pub roots: Option<ProviderRoots>,
    /// Keeps a staged artifact on disk while the evidence is analyzed.
    _staged: Option<Staged>,
}

pub(crate) fn load_session(
    locator: &SessionLocator,
    store: &HistoryStoreOptions,
) -> Result<LoadedSession> {
    match locator {
        SessionLocator::Id {
            harness,
            session_id,
        } => load_by_id(*harness, session_id, store),
        SessionLocator::Path { harness, path } => load_by_path(*harness, path),
    }
}

pub(crate) fn source_of(harness: Harness) -> Source {
    match harness {
        Harness::ClaudeCode => Source::Claude,
        Harness::Codex => Source::Codex,
        Harness::Opencode => Source::OpenCode,
    }
}

/// Open (creating it on first use) the relayhistory store `opts` names.
pub(crate) fn open_store(opts: &HistoryStoreOptions) -> Result<SessionStore> {
    SessionStore::open(store_options(opts, false)).context("open relayhistory store")
}

/// The relayhistory store `opts` names, read-only; an error when it does
/// not exist yet.
pub(crate) fn open_existing_store(opts: &HistoryStoreOptions) -> Result<SessionStore> {
    SessionStore::open(store_options(opts, true)).context("open relayhistory store")
}

/// An explicit provider home is the whole layout: its harness stores are
/// read from under it, whatever the process environment says.
fn store_options(opts: &HistoryStoreOptions, read_only: bool) -> StoreOptions {
    let mut options = StoreOptions::default();
    options.db_path = opts.db_path.clone();
    options.home = opts.home.clone();
    options.roots = opts.home.as_ref().map(|home| {
        ProviderRoots::from_home(home.clone(), home.join(".local/share/opencode/opencode.db"))
    });
    options.read_only = read_only;
    options
}

fn load_by_id(
    harness: Harness,
    session_id: &str,
    opts: &HistoryStoreOptions,
) -> Result<LoadedSession> {
    let store = open_store(opts)?;
    let reference = SessionRef::id(source_of(harness), session_id);
    hydrate_catalogued(&store, &reference).map_err(|error| {
        anyhow!(
            "{harness} session {session_id} is not in the relayhistory store {} or under {}: {error}",
            store.db_path().display(),
            store.roots().home.display(),
        )
    })?;
    let evidence = read_evidence(&store, &reference)?;
    Ok(LoadedSession {
        children: delegated_children(&store, &evidence)?,
        evidence,
        roots: Some(store.roots().clone()),
        _staged: None,
    })
}

/// Hydrate `reference`, discovering its harness first when the store has
/// never catalogued it.
fn hydrate_catalogued(store: &SessionStore, reference: &SessionRef) -> Result<()> {
    let status = match store.hydrate(reference, HydrateOptions::default()) {
        Err(ai_hist::Error::SessionNotFound(_)) => {
            discover(store, reference.source())?;
            store.hydrate(reference, HydrateOptions::default())?.status
        }
        other => other?.status,
    };
    check_hydrated(status)
}

pub(super) fn discover(store: &SessionStore, source: Source) -> Result<()> {
    let mut options = DiscoveryOptions::default();
    options.sources = Some(vec![source]);
    store
        .discover(options)
        .with_context(|| format!("discover {source} sessions"))?;
    Ok(())
}

fn check_hydrated(status: HydrateStatus) -> Result<()> {
    match status {
        HydrateStatus::Missing => bail!("the transcript is missing"),
        HydrateStatus::Unidentified => {
            bail!("the transcript carries no session identity (a subagent sidecar, or still being written)")
        }
        HydrateStatus::Mismatched => bail!("the transcript names a different session"),
        _ => Ok(()),
    }
}

pub(super) fn read_evidence(
    store: &SessionStore,
    reference: &SessionRef,
) -> Result<SessionEvidence> {
    store
        .session(reference, SessionQuery::default())
        .context("read session evidence")?
        .ok_or_else(|| anyhow!("relayhistory holds no evidence for {reference:?}"))
}

fn load_by_path(harness: Harness, path: &std::path::Path) -> Result<LoadedSession> {
    if !path.is_file() {
        bail!("session input is not a file: {}", path.display());
    }
    let staged = stage_path(harness, path)?;
    let store = staged.open_store()?;
    let reference = staged
        .hydrate(&store)
        .map_err(|error| anyhow!("read {harness} session from {}: {error:#}", path.display()))?;
    let evidence = read_evidence(&store, &reference)?;
    Ok(LoadedSession {
        children: delegated_children(&store, &evidence)?,
        evidence,
        roots: staged.install_roots(),
        _staged: Some(staged),
    })
}

pub(super) fn hydrate_path(store: &SessionStore, reference: &SessionRef) -> Result<SessionRef> {
    let report = store.hydrate(reference, HydrateOptions::default())?;
    check_hydrated(report.status)?;
    Ok(report.session)
}
