//! Lay one session artifact out where relayhistory reads it.
//!
//! relayhistory reads sessions from provider roots, so a path is analyzed
//! through a throwaway store whose roots hold exactly that session:
//!
//! - **Claude Code** — a transcript already inside a Claude install
//!   (`<root>/projects/<project>/<id>.jsonl`) is read in place, sidecars
//!   included. Any other transcript is linked into a staged
//!   `projects/` directory together with its `<id>/` sidecar directory.
//! - **Codex** — the rollout is linked into a staged `sessions/` tree under
//!   a `rollout-*.jsonl` name and read by the thread id its `session_meta`
//!   declares, a subagent thread's rollout included. Child rollouts it
//!   spawned are not staged.
//! - **OpenCode** — the session metadata file names its `storage/` tree,
//!   which is read in place.

use std::fs;
use std::path::{Path, PathBuf};

use ai_hist::{ProviderRoots, SessionRef, SessionStore, Source, StoreOptions, SyncOptions};
use anyhow::{anyhow, Context, Result};
use tempfile::TempDir;

use super::locate::{discover, hydrate_path, source_of};
use crate::reader::Harness;

/// A staged artifact and the throwaway store that reads it.
pub(crate) struct Staged {
    dir: TempDir,
    roots: ProviderRoots,
    /// Whether `roots` is the harness install the artifact lives in.
    in_install: bool,
    target: Target,
}

enum Target {
    /// A Claude transcript, hydrated by its path.
    Transcript(PathBuf),
    /// A session of `source` named by id.
    Session(Source, String),
    /// A Codex thread named by id, captured by a full sweep of the staged
    /// roots: a subagent thread is a delegated child the catalog leaves out,
    /// so only the sweep reads it, and it is then read by id like any other.
    Thread(String),
}

pub(super) fn stage_path(harness: Harness, path: &Path) -> Result<Staged> {
    let dir = tempfile::tempdir().context("create staging directory")?;
    let mut roots = staged_roots(dir.path());
    let (in_install, target) = match harness {
        Harness::ClaudeCode => stage_claude(&mut roots, path)?,
        Harness::Codex => (false, stage_codex(&roots, path)?),
        Harness::Opencode => (false, stage_opencode(&mut roots, path)?),
    };
    Ok(Staged {
        dir,
        roots,
        in_install,
        target,
    })
}

impl Staged {
    pub(super) fn open_store(&self) -> Result<SessionStore> {
        let mut options = StoreOptions::default();
        options.db_path = Some(self.dir.path().join("ai-history.db"));
        options.roots = Some(self.roots.clone());
        SessionStore::open(options).context("open staging relayhistory store")
    }

    /// Hydrate the staged session and return its id reference.
    pub(super) fn hydrate(&self, store: &SessionStore) -> Result<SessionRef> {
        let reference = match &self.target {
            Target::Transcript(path) => SessionRef::path(Source::Claude, path),
            Target::Session(source, id) => {
                discover(store, *source)?;
                SessionRef::id(*source, id)
            }
            Target::Thread(id) => {
                store
                    .sync(SyncOptions::default())
                    .context("capture the staged codex rollout")?;
                return Ok(SessionRef::id(Source::Codex, id));
            }
        };
        hydrate_path(store, &reference)
    }

    pub(super) fn install_roots(&self) -> Option<ProviderRoots> {
        self.in_install.then(|| self.roots.clone())
    }
}

fn staged_roots(home: &Path) -> ProviderRoots {
    ProviderRoots::from_home(
        home.to_path_buf(),
        home.join(".local/share/opencode/opencode.db"),
    )
}

/// `(in install, target)` for a Claude transcript.
fn stage_claude(roots: &mut ProviderRoots, path: &Path) -> Result<(bool, Target)> {
    if let Some(root) = claude_install_root(path) {
        roots.claude = root;
        return Ok((true, Target::Transcript(path.to_path_buf())));
    }
    let project = roots.claude.join("projects").join("-staged");
    fs::create_dir_all(&project)?;
    let transcript = project.join(file_name(path)?);
    link_or_copy(path, &transcript)?;
    let sidecars = path.with_extension("");
    if sidecars.is_dir() {
        link_tree(&sidecars, &project.join(file_name(&sidecars)?))?;
    }
    Ok((false, Target::Transcript(transcript)))
}

/// `<root>` for a transcript at `<root>/projects/<project>/<file>`.
fn claude_install_root(path: &Path) -> Option<PathBuf> {
    let projects = path.parent()?.parent()?;
    (projects.file_name()? == "projects").then(|| projects.parent().map(Path::to_path_buf))?
}

fn stage_codex(roots: &ProviderRoots, path: &Path) -> Result<Target> {
    let day = roots.codex.join("sessions/2000/01/01");
    fs::create_dir_all(&day)?;
    let name = file_name(path)?;
    let name = if name.starts_with("rollout-") && name.ends_with(".jsonl") {
        name.to_string()
    } else {
        format!("rollout-{name}.jsonl")
    };
    link_or_copy(path, &day.join(name))?;
    Ok(Target::Thread(codex_session_id(path)?))
}

fn stage_opencode(roots: &mut ProviderRoots, path: &Path) -> Result<Target> {
    let session_dir = path.parent().and_then(Path::parent);
    let storage = session_dir
        .filter(|dir| dir.file_name().is_some_and(|name| name == "session"))
        .and_then(Path::parent)
        .ok_or_else(|| {
            anyhow!(
                "OpenCode input must be a session metadata file inside its storage tree \
                 (storage/session/<scope>/<sessionId>.json), got {}",
                path.display()
            )
        })?;
    roots.opencode_storage_dir = storage.to_path_buf();
    Ok(Target::Session(
        source_of(Harness::Opencode),
        opencode_session_id(path)?,
    ))
}

/// The thread id a Codex rollout's opening `session_meta` declares.
fn codex_session_id(path: &Path) -> Result<String> {
    use std::io::BufRead;
    let file = fs::File::open(path)?;
    for line in std::io::BufReader::new(file)
        .lines()
        .take(SESSION_META_LINES)
    {
        let value: serde_json::Value = match serde_json::from_str(&line?) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        if let Some(id) = value.pointer("/payload/id").and_then(|id| id.as_str()) {
            return Ok(id.to_string());
        }
    }
    Err(anyhow!("relayhistory recognized no codex session in the file: it opens with no session_meta naming a thread id"))
}

/// Leading rollout lines searched for the `session_meta` record.
const SESSION_META_LINES: usize = 16;

/// The `id` an OpenCode session metadata file declares, else its file stem.
fn opencode_session_id(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path)?;
    let declared = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("id")?.as_str().map(str::to_string));
    match declared {
        Some(id) => Ok(id),
        None => Ok(path
            .file_stem()
            .ok_or_else(|| anyhow!("OpenCode input has no file name"))?
            .to_string_lossy()
            .into_owned()),
    }
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("{} has no UTF-8 file name", path.display()))
}

/// Hard-link `src` to `dst`, copying when the two sit on different volumes.
fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst)
            .with_context(|| format!("stage {} into {}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let path = entry?.path();
        let target = dst.join(file_name(&path)?);
        if path.is_dir() {
            link_tree(&path, &target)?;
        } else if path.is_file() {
            link_or_copy(&path, &target)?;
        }
    }
    Ok(())
}
