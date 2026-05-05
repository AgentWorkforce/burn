//! User-level config for the `burn` toolchain.
//!
//! Mirrors `packages/ledger/src/config.ts`: a small JSON file at
//! `$RELAYBURN_HOME/config.json` with environment-variable overrides for
//! the two knobs ingest cares about (`content.store` and
//! `content.retentionDays`). The TS source-of-truth co-locates this with
//! `@relayburn/ledger`, so the Rust port keeps the same home — the
//! ledger crate already depends on `relayburn-reader` for
//! [`ContentStoreMode`], and ingest (#277, #278) already depends on the
//! ledger, so no new edge in the dependency graph.
//!
//! Schema: keep the field names byte-equivalent to TS so a config file
//! authored against either tree is read identically by the other while
//! the dual tree coexists on `main`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use relayburn_reader::ContentStoreMode;

use crate::error::Result;
use crate::paths::ledger_home;

/// Default content retention window in days. Matches TS
/// `DEFAULT_RETENTION_DAYS`.
pub const DEFAULT_RETENTION_DAYS: u64 = 90;

/// Retention window for content rows. Mirrors the TS
/// `number | 'forever'` shape; `Forever` disables TTL-based pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Retain content for at most `days` days.
    Days(u64),
    /// Retain content indefinitely.
    Forever,
}

impl Retention {
    /// Convert the retention window to milliseconds. Mirrors TS
    /// `retentionMs`. Returns `None` for `Forever` (no cutoff).
    pub fn as_millis(self) -> Option<u64> {
        match self {
            Retention::Forever => None,
            Retention::Days(d) => Some(d.saturating_mul(24 * 60 * 60 * 1000)),
        }
    }
}

/// `content.*` block of the resolved config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentConfig {
    pub store: ContentStoreMode,
    pub retention_days: Retention,
}

/// Resolved user config, with all defaults applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnConfig {
    pub content: ContentConfig,
}

impl Default for BurnConfig {
    /// Defaults match TS `DEFAULT_CONFIG`:
    /// `{ content: { store: 'full', retentionDays: 90 } }`.
    fn default() -> Self {
        Self {
            content: ContentConfig {
                store: ContentStoreMode::Full,
                retention_days: Retention::Days(DEFAULT_RETENTION_DAYS),
            },
        }
    }
}

/// Path to the JSON config file. `$RELAYBURN_HOME/config.json`, mirroring
/// TS `configPath()`.
pub fn config_path() -> PathBuf {
    ledger_home().join("config.json")
}

/// Load the user config: read the JSON file (if present), then layer the
/// `RELAYBURN_CONTENT_STORE` and `RELAYBURN_CONTENT_TTL_DAYS` env vars on
/// top, falling back to [`BurnConfig::default`].
///
/// Mirrors the TS `loadConfig()` precedence: env overrides file overrides
/// default. A missing file is the common case and not an error; malformed
/// JSON is logged to stderr and treated as if the file were absent —
/// same fail-soft behaviour the TS surface has.
pub fn load_config() -> Result<BurnConfig> {
    load_config_at(&config_path())
}

/// Load with an explicit config path. Tests use this to avoid touching
/// `$HOME/.relayburn/config.json`.
pub fn load_config_at(path: &Path) -> Result<BurnConfig> {
    let from_file = read_config_file(path);
    let store = pick_store(
        std::env::var("RELAYBURN_CONTENT_STORE").ok().as_deref(),
        from_file
            .as_ref()
            .and_then(|c| c.content.as_ref())
            .and_then(|c| c.store.as_ref()),
    );
    let retention = pick_retention(
        std::env::var("RELAYBURN_CONTENT_TTL_DAYS").ok().as_deref(),
        from_file
            .as_ref()
            .and_then(|c| c.content.as_ref())
            .and_then(|c| c.retention_days.as_ref()),
    );
    Ok(BurnConfig {
        content: ContentConfig {
            store,
            retention_days: retention,
        },
    })
}

// --- raw JSON shape ---------------------------------------------------

/// Permissive on-disk shape: every leaf is a `serde_json::Value` so a
/// malformed-but-parseable field just falls back to default rather than
/// failing the whole load. Matches the TS `RawConfig` interface, which
/// accepts unknowns and lets the picker functions normalize.
#[derive(Debug, Default, Deserialize, Serialize)]
struct RawConfig {
    #[serde(default)]
    content: Option<RawContent>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawContent {
    #[serde(default)]
    store: Option<serde_json::Value>,
    #[serde(default, rename = "retentionDays")]
    retention_days: Option<serde_json::Value>,
}

fn read_config_file(path: &Path) -> Option<RawConfig> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            // Missing config file is the common case and not worth
            // mentioning — same fail-quiet behaviour as TS.
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[burn] warning: could not read {}: {}",
                    path.display(),
                    err
                );
            }
            return None;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) if v.is_object() => match serde_json::from_value::<RawConfig>(v) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                eprintln!(
                    "[burn] warning: invalid config shape in {} ({}); using defaults",
                    path.display(),
                    err
                );
                None
            }
        },
        Ok(_) => {
            eprintln!(
                "[burn] warning: {} is not a JSON object; using defaults",
                path.display()
            );
            None
        }
        Err(err) => {
            eprintln!(
                "[burn] warning: invalid JSON in {} ({}); using defaults",
                path.display(),
                err
            );
            None
        }
    }
}

// --- picker helpers ---------------------------------------------------

fn pick_store(env: Option<&str>, from_file: Option<&serde_json::Value>) -> ContentStoreMode {
    if let Some(s) = normalize_store_str(env) {
        return s;
    }
    if let Some(s) = normalize_store_value(from_file) {
        return s;
    }
    BurnConfig::default().content.store
}

fn normalize_store_value(v: Option<&serde_json::Value>) -> Option<ContentStoreMode> {
    match v? {
        serde_json::Value::String(s) => normalize_store_str(Some(s)),
        _ => None,
    }
}

fn normalize_store_str(v: Option<&str>) -> Option<ContentStoreMode> {
    let s = v?.to_ascii_lowercase();
    match s.as_str() {
        "full" => Some(ContentStoreMode::Full),
        "hash-only" => Some(ContentStoreMode::HashOnly),
        "off" => Some(ContentStoreMode::Off),
        _ => None,
    }
}

fn pick_retention(env: Option<&str>, from_file: Option<&serde_json::Value>) -> Retention {
    if let Some(r) = normalize_retention_str(env) {
        return r;
    }
    if let Some(r) = normalize_retention_value(from_file) {
        return r;
    }
    BurnConfig::default().content.retention_days
}

fn normalize_retention_value(v: Option<&serde_json::Value>) -> Option<Retention> {
    match v? {
        serde_json::Value::String(s) => normalize_retention_str(Some(s)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i < 0 {
                    return Some(Retention::Forever);
                }
                return Some(Retention::Days(i as u64));
            }
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return None;
                }
                if f < 0.0 {
                    return Some(Retention::Forever);
                }
                return Some(Retention::Days(f as u64));
            }
            None
        }
        _ => None,
    }
}

fn normalize_retention_str(v: Option<&str>) -> Option<Retention> {
    let trimmed = v?.trim();
    // Empty string means "not set" — important because
    // `RELAYBURN_CONTENT_TTL_DAYS=` (or a CI/CD pipeline producing an
    // empty value) would otherwise parse as 0 and silently configure a
    // zero-day retention, mirroring the TS guard.
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("forever") {
        return Some(Retention::Forever);
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        if i < 0 {
            return Some(Retention::Forever);
        }
        return Some(Retention::Days(i as u64));
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        if !f.is_finite() {
            return None;
        }
        if f < 0.0 {
            return Some(Retention::Forever);
        }
        return Some(Retention::Days(f as u64));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // The picker functions read process-wide env vars. Serialize tests
    // that touch them so a parallel test run doesn't see a leaked value
    // from a peer.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_clean_env<F: FnOnce()>(f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("RELAYBURN_CONTENT_STORE");
        std::env::remove_var("RELAYBURN_CONTENT_TTL_DAYS");
        f();
        std::env::remove_var("RELAYBURN_CONTENT_STORE");
        std::env::remove_var("RELAYBURN_CONTENT_TTL_DAYS");
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let cfg = load_config_at(&tmp.path().join("config.json")).unwrap();
            assert_eq!(cfg.content.store, ContentStoreMode::Full);
            assert_eq!(cfg.content.retention_days, Retention::Days(90));
            assert_eq!(cfg, BurnConfig::default());
        });
    }

    #[test]
    fn file_overrides_default() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                r#"{"content":{"store":"hash-only","retentionDays":7}}"#,
            )
            .unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg.content.store, ContentStoreMode::HashOnly);
            assert_eq!(cfg.content.retention_days, Retention::Days(7));
        });
    }

    #[test]
    fn file_forever_string_disables_ttl() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                r#"{"content":{"store":"full","retentionDays":"forever"}}"#,
            )
            .unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg.content.retention_days, Retention::Forever);
        });
    }

    #[test]
    fn file_negative_retention_means_forever() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                r#"{"content":{"store":"full","retentionDays":-1}}"#,
            )
            .unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg.content.retention_days, Retention::Forever);
        });
    }

    #[test]
    fn env_overrides_file() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                r#"{"content":{"store":"hash-only","retentionDays":7}}"#,
            )
            .unwrap();
            std::env::set_var("RELAYBURN_CONTENT_STORE", "off");
            std::env::set_var("RELAYBURN_CONTENT_TTL_DAYS", "30");
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg.content.store, ContentStoreMode::Off);
            assert_eq!(cfg.content.retention_days, Retention::Days(30));
        });
    }

    #[test]
    fn empty_env_string_does_not_zero_retention() {
        with_clean_env(|| {
            // CI pipelines that emit `RELAYBURN_CONTENT_TTL_DAYS=` would
            // otherwise yield a zero-day retention; guard against it.
            std::env::set_var("RELAYBURN_CONTENT_TTL_DAYS", "");
            let tmp = TempDir::new().unwrap();
            let cfg = load_config_at(&tmp.path().join("missing.json")).unwrap();
            assert_eq!(cfg.content.retention_days, Retention::Days(90));
        });
    }

    #[test]
    fn malformed_json_falls_back_to_default() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(&path, "not json").unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg, BurnConfig::default());
        });
    }

    #[test]
    fn non_object_json_falls_back_to_default() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(&path, "[1,2,3]").unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg, BurnConfig::default());
        });
    }

    #[test]
    fn unknown_store_value_falls_back() {
        with_clean_env(|| {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.json");
            std::fs::write(
                &path,
                r#"{"content":{"store":"bogus","retentionDays":7}}"#,
            )
            .unwrap();
            let cfg = load_config_at(&path).unwrap();
            assert_eq!(cfg.content.store, ContentStoreMode::Full);
            assert_eq!(cfg.content.retention_days, Retention::Days(7));
        });
    }

    #[test]
    fn retention_as_millis() {
        assert_eq!(Retention::Forever.as_millis(), None);
        assert_eq!(Retention::Days(0).as_millis(), Some(0));
        assert_eq!(
            Retention::Days(1).as_millis(),
            Some(24 * 60 * 60 * 1000),
        );
    }
}
