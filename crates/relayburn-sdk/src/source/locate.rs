//! Resolve one harness session into relayhistory evidence.
//!
//! An id is looked up in a relayhistory store: hydrated when catalogued,
//! otherwise discovered for that harness alone and then hydrated (a Codex
//! subagent thread, which discovery does not catalog, by a sweep of the
//! Codex roots). A path is
//! read through a throwaway store, so analyzing a transcript persists
//! nothing; see [`super::stage`] for how each harness's artifact is laid out
//! for relayhistory.

use std::path::PathBuf;

use ai_hist::{
    DiscoveryOptions, HydrateOptions, HydrateStatus, ProviderRoots, SessionEvidence, SessionQuery,
    SessionRef, SessionStore, Source, StoreOptions, SyncOptions,
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
/// never catalogued it. Discovery leaves Codex subagent threads out of the
/// catalog, so a Codex id discovery does not find is captured by a sweep of
/// the Codex roots, which reads every thread in full.
fn hydrate_catalogued(store: &SessionStore, reference: &SessionRef) -> Result<()> {
    let hydrate = || store.hydrate(reference, HydrateOptions::default());
    let status = match hydrate() {
        Err(ai_hist::Error::SessionNotFound(_)) => {
            discover(store, reference.source())?;
            match hydrate() {
                Err(ai_hist::Error::SessionNotFound(_)) if reference.source() == Source::Codex => {
                    return sweep_codex(store, reference);
                }
                other => other?.status,
            }
        }
        other => other?.status,
    };
    check_hydrated(status)
}

fn sweep_codex(store: &SessionStore, reference: &SessionRef) -> Result<()> {
    let mut options = SyncOptions::default();
    options.sources = Some(vec![Source::Codex]);
    store.sync(options).context("capture codex threads")?;
    let mut catalog_only = SessionQuery::default();
    catalog_only.include_text = false;
    catalog_only.kinds = Some(Vec::new());
    match store.session(reference, catalog_only)? {
        Some(_) => Ok(()),
        None => bail!("no codex thread has that id"),
    }
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
        .session(reference, super::records_query())
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

#[cfg(test)]
mod tests {
    use ai_hist::{HydrateStatus, SessionStore, StoreOptions};

    use super::{check_hydrated, load_session, open_existing_store, open_store};
    use super::{HistoryStoreOptions, SessionLocator};
    use crate::reader::Harness;

    #[test]
    fn only_a_transcript_naming_its_session_hydrates() {
        for ok in [
            HydrateStatus::Hydrated,
            HydrateStatus::Updated,
            HydrateStatus::Unchanged,
        ] {
            assert!(check_hydrated(ok).is_ok());
        }
        let reason = |status| check_hydrated(status).unwrap_err().to_string();
        assert_eq!(reason(HydrateStatus::Missing), "the transcript is missing");
        assert!(reason(HydrateStatus::Unidentified)
            .starts_with("the transcript carries no session identity"));
        assert_eq!(
            reason(HydrateStatus::Mismatched),
            "the transcript names a different session"
        );
    }

    /// The user's real ai-hist files, fingerprinted read-only.
    fn real_store_fingerprint() -> Vec<(std::path::PathBuf, Option<(u64, std::time::SystemTime)>)> {
        let Some(home) = relayburn_test_seal::original_home() else {
            return Vec::new();
        };
        ["ai-history.db", "ai-history.db-wal", ".sync-state.json"]
            .iter()
            .map(|name| {
                let path = home.join(".local/share/ai-hist").join(name);
                let meta = std::fs::metadata(&path).ok();
                let print = meta.map(|m| (m.len(), m.modified().unwrap()));
                (path, print)
            })
            .collect()
    }

    /// The error `opened` must be: anything but a store opened from a
    /// default path, and failing at the sealed `HOME`.
    fn assert_fails_at_the_tripwire<T>(what: &str, opened: anyhow::Result<T>) {
        let Err(error) = opened else {
            panic!("{what} opened a default store inside a test");
        };
        let chain = format!("{error:#}");
        let tripwire = relayburn_test_seal::tripwire().display().to_string();
        assert!(
            chain.contains(&tripwire) || chain.contains("Not a directory"),
            "{what} failed for some other reason: {chain}"
        );
    }

    /// A test that forgets its store paths — or a mutant that drops them,
    /// as `store_options -> Default::default()` does — resolves every
    /// default under the sealed `HOME` and fails there, leaving the user's
    /// real ai-hist store untouched.
    #[test]
    fn forgotten_store_paths_fail_loudly_instead_of_opening_the_real_store() {
        let _env = crate::ledger::CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = real_store_fingerprint();

        assert_fails_at_the_tripwire(
            "StoreOptions::default()",
            SessionStore::open(StoreOptions::default()).map_err(anyhow::Error::from),
        );
        let forgotten = HistoryStoreOptions::default();
        assert_fails_at_the_tripwire("open_store", open_store(&forgotten));
        assert_fails_at_the_tripwire("open_existing_store", open_existing_store(&forgotten));
        let by_id = SessionLocator::Id {
            harness: Harness::ClaudeCode,
            session_id: "forgotten-store".into(),
        };
        assert_fails_at_the_tripwire("load_session", load_session(&by_id, &forgotten));

        assert_eq!(real_store_fingerprint(), before);
    }
}
