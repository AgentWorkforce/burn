//! Claude harness adapter — Rust port of
//! `packages/cli/src/harnesses/claude.ts`.
//!
//! Claude is the simplest of the three production harnesses and serves
//! as the canonical "eager / unit-struct adapter" example for the
//! [`super::registry::EAGER_ADAPTERS`] tier:
//!
//! - **`plan`** mints a fresh session id (UUID v4) and injects it via
//!   `--session-id`, plus exports `RELAYBURN_SESSION_ID` so any nested
//!   `burn …` invocation inside the child sees the same id.
//! - **`before_spawn`** stamps the session up front with the user's
//!   enrichment tags. The session id is final from the moment the child
//!   spawns, so we don't need a pending-stamp manifest like
//!   codex/opencode.
//! - **`start_watcher`** is left at the default `None`. Claude writes
//!   exactly one JSONL file per session at `~/.claude/projects/<cwd>/<sid>.jsonl`,
//!   and the post-exit fast-path
//!   ([`relayburn_sdk::ingest_claude_session`]) reads it directly. There
//!   is nothing for a watch loop to drain.
//! - **`after_exit`** runs the per-session fast-path against the known
//!   sessionId.
//!
//! The adapter itself is a zero-sized unit struct; the static
//! [`CLAUDE_ADAPTER`] handed to [`super::registry::EAGER_ADAPTERS`] is a
//! compile-time `&'static dyn HarnessAdapter` reference, so harness
//! lookup costs nothing at startup.

use std::path::PathBuf;

use async_trait::async_trait;
use relayburn_sdk::{
    ingest_claude_session, Enrichment, IngestReport, Ledger, LedgerOpenOptions, RawIngestOptions,
    Stamp, StampSelector,
};
use uuid::Uuid;

use super::{HarnessAdapter, PlanCtx, SpawnPlan};
use crate::util::home::home_dir;
use crate::util::time::iso_now;

/// Public unit-struct adapter for `claude`. Held as `&'static
/// CLAUDE_ADAPTER` in the eager `phf::Map` registry — the value `&CLAUDE_ADAPTER`
/// is a const expression so it satisfies `phf_map!`'s value bound directly.
pub struct ClaudeAdapter;

/// Static singleton handed to the eager registry. Lifetime: `'static`,
/// stateless; cloning is unnecessary.
pub static CLAUDE_ADAPTER: ClaudeAdapter = ClaudeAdapter;

/// Default Claude session-store root: `$HOME/.claude/projects`.
fn claude_projects_root() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

/// Mint a v4 UUID the child Claude binary will adopt as its session id.
/// The SDK validates the shape via [`relayburn_sdk::is_valid_session_id`]
/// when it stamps.
fn mint_session_id() -> String {
    Uuid::new_v4().to_string()
}

#[async_trait]
impl HarnessAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn session_root(&self) -> PathBuf {
        claude_projects_root()
    }

    async fn plan(&self, ctx: &PlanCtx) -> anyhow::Result<SpawnPlan> {
        let session_id = mint_session_id();
        let mut args = vec!["--session-id".to_string(), session_id.clone()];
        args.extend(ctx.passthrough.iter().cloned());
        Ok(SpawnPlan {
            binary: "claude".to_string(),
            args,
            env_overrides: vec![("RELAYBURN_SESSION_ID".to_string(), session_id.clone())],
            session_id: Some(session_id),
        })
    }

    async fn before_spawn(&self, ctx: &PlanCtx, plan: &SpawnPlan) -> anyhow::Result<()> {
        let session_id = plan
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("claude adapter: plan must include sessionId"))?;
        write_session_stamp(session_id, &ctx.tags)?;
        eprintln!("[burn] session-id={session_id}");
        Ok(())
    }

    async fn after_exit(&self, ctx: &PlanCtx, plan: &SpawnPlan) -> anyhow::Result<IngestReport> {
        let session_id = plan
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("claude adapter: plan must include sessionId"))?;
        // Open a ledger handle scoped to the resolved RELAYBURN_HOME and
        // run the per-session fast-path. The SDK encodes cwd → flattened
        // dir name internally and persists a cursor at EOF so the next
        // sweep skips the file.
        let mut handle = Ledger::open(LedgerOpenOptions::default())?;
        let cwd_str = ctx.cwd.to_string_lossy().into_owned();
        let opts = RawIngestOptions::default();
        ingest_claude_session(handle.raw_mut(), &cwd_str, session_id, &opts)
    }
}

/// Append a session stamp via the SDK ledger. Mirrors the TS sibling's
/// `await stamp({ sessionId }, ctx.tags)` call, but goes through the
/// Rust SDK's typed `Stamp::new` + `Ledger::append_stamp` pair.
fn write_session_stamp(session_id: &str, enrichment: &Enrichment) -> anyhow::Result<()> {
    let mut handle = Ledger::open(LedgerOpenOptions::default())?;
    let selector = StampSelector {
        session_id: Some(session_id.to_string()),
        ..Default::default()
    };
    let stamp = Stamp::new(iso_now(), selector, enrichment.clone())?;
    handle.raw_mut().append_stamp(&stamp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[tokio::test]
    async fn plan_mints_session_id_and_prepends_session_id_arg() {
        let ctx = PlanCtx {
            cwd: PathBuf::from("/tmp"),
            passthrough: vec!["--resume".to_string(), "abc".to_string()],
            tags: Enrichment::new(),
            ledger_home: None,
            spawn_start_ts: std::time::SystemTime::now(),
        };
        let plan = CLAUDE_ADAPTER.plan(&ctx).await.unwrap();
        assert_eq!(plan.binary, "claude");
        assert_eq!(plan.args[0], "--session-id");
        let sid = plan.args.get(1).cloned().unwrap_or_default();
        assert!(plan.session_id.as_deref() == Some(sid.as_str()));
        assert_eq!(
            &plan.args[2..],
            &["--resume".to_string(), "abc".to_string()]
        );
        // Env override carries the same id so a nested `burn …` inherits it.
        assert!(plan
            .env_overrides
            .iter()
            .any(|(k, v)| k == "RELAYBURN_SESSION_ID" && v == &sid));
    }

    #[test]
    fn name_is_claude_lowercase() {
        assert_eq!(CLAUDE_ADAPTER.name(), "claude");
    }

    #[test]
    fn session_root_lands_under_dot_claude_projects() {
        let root = CLAUDE_ADAPTER.session_root();
        let s = root.to_string_lossy();
        assert!(
            s.ends_with(".claude/projects") || s.ends_with(".claude\\projects"),
            "expected session_root under .claude/projects, got {s}"
        );
    }

    #[test]
    fn mint_session_id_round_trips_a_v4_uuid_shape() {
        let s = mint_session_id();
        // 8-4-4-4-12 hex.
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version nibble = 4.
        assert_eq!(&parts[2][..1], "4", "version nibble should be 4 in {s}");
        // Variant bits: top two bits of the first nibble of `parts[3]` are 10.
        let variant_nibble = u8::from_str_radix(&parts[3][..1], 16).unwrap();
        assert_eq!(variant_nibble & 0xC, 0x8, "variant nibble should be 10xx");
    }

    #[test]
    fn iso_now_is_zulu_iso8601() {
        let s = iso_now();
        // Coarse shape: YYYY-MM-DDTHH:MM:SS.mmmZ
        assert_eq!(s.len(), "1970-01-01T00:00:00.000Z".len());
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert_eq!(&s[19..20], ".");
    }
}
