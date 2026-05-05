//! Export + search verbs — `search`, `export_ledger`, `export_stamps`.
//!
//! Wraps the FTS5 search and JSONL export APIs on
//! [`relayburn_ledger::Ledger`]. Each verb appears as an
//! [`LedgerHandle`] method (sync, returns [`anyhow::Result`]) plus a
//! free-function form that opens its own handle from a
//! [`LedgerOpenOptions`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Ledger, LedgerHandle, LedgerOpenOptions, SearchHit, SearchOptions};

// --- search ----------------------------------------------------------------

/// Options for the FTS5 search verb. Equivalent to
/// [`relayburn_ledger::SearchOptions`] but owns its strings so callers can
/// build it without juggling lifetimes.
#[derive(Debug, Clone)]
pub struct SearchQueryOptions {
    /// FTS5 query string. Supports phrase (`"out of memory"`), boolean
    /// (`a OR b`), and prefix (`mem*`) syntax — see the SQLite docs.
    pub query: String,
    /// Hit cap. `None` defers to the lower-crate default (25).
    pub limit: Option<usize>,
    /// Restrict to a single session_id. `None` searches all sessions.
    pub session_id: Option<String>,
    /// Override for `$RELAYBURN_HOME`. Only consulted by the
    /// free-function form; the method form uses the handle's existing
    /// connections.
    pub ledger_home: Option<PathBuf>,
}

impl Default for SearchQueryOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: None,
            session_id: None,
            ledger_home: None,
        }
    }
}

impl SearchQueryOptions {
    /// Convenience constructor matching the lower-crate
    /// [`SearchOptions::new`] shape.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }
}

/// Result of a [`search`] call. Carries the original query alongside the
/// hits so consumers (CLI tables, MCP tool responses) can echo it back
/// without re-threading state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

impl LedgerHandle {
    /// Run an FTS5 search over the content store. Returns BM25-ranked
    /// hits (lower rank = better match) with `<b>…</b>`-highlighted
    /// snippets.
    pub fn search(&self, opts: SearchQueryOptions) -> anyhow::Result<SearchResult> {
        // SearchOptions borrows from `opts`; build it inline so the
        // lifetime is tied to this call.
        let mut lower = SearchOptions::new(&opts.query);
        if let Some(limit) = opts.limit {
            lower.limit = limit;
        }
        if let Some(sid) = opts.session_id.as_deref() {
            lower.session_id = Some(sid);
        }
        let hits = self.inner.search_content(lower)?;
        Ok(SearchResult {
            query: opts.query,
            hits,
        })
    }
}

/// Free-function form of [`LedgerHandle::search`] — opens a ledger from
/// `opts.ledger_home` (or the env-var default) and runs the search.
pub fn search(opts: SearchQueryOptions) -> anyhow::Result<SearchResult> {
    let handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        content_home: None,
    })?;
    handle.search(SearchQueryOptions {
        ledger_home: None,
        ..opts
    })
}

// --- export_ledger ---------------------------------------------------------

/// Options for [`export_ledger`].
#[derive(Debug, Default, Clone)]
pub struct ExportLedgerOptions {
    /// Override for `$RELAYBURN_HOME`. Only consulted by the
    /// free-function form.
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Stream every event row as a JSONL-shaped [`serde_json::Value`].
    /// Each value has the form `{"v":1,"kind":"<kind>","record":<json>}`,
    /// matching the bytes [`relayburn_ledger::Ledger::export_ledger_jsonl`]
    /// would write.
    ///
    /// Buffered into a `Vec` for v1 (relayburn ledgers are small enough
    /// that this is fine); the iterator surface lets us add a streaming
    /// variant later without breaking callers.
    pub fn export_ledger(
        &self,
        _opts: ExportLedgerOptions,
    ) -> anyhow::Result<impl Iterator<Item = serde_json::Value>> {
        let mut buf: Vec<u8> = Vec::new();
        self.inner.export_ledger_jsonl(&mut buf)?;
        Ok(parse_jsonl_lines(&buf)?.into_iter())
    }
}

/// Free-function form of [`LedgerHandle::export_ledger`].
pub fn export_ledger(
    opts: ExportLedgerOptions,
) -> anyhow::Result<impl Iterator<Item = serde_json::Value>> {
    let handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        content_home: None,
    })?;
    let mut buf: Vec<u8> = Vec::new();
    handle.inner.export_ledger_jsonl(&mut buf)?;
    Ok(parse_jsonl_lines(&buf)?.into_iter())
}

// --- export_stamps ---------------------------------------------------------

/// Options for [`export_stamps`].
#[derive(Debug, Default, Clone)]
pub struct ExportStampsOptions {
    /// Override for `$RELAYBURN_HOME`. Only consulted by the
    /// free-function form.
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Stream every stamp row as a JSONL-shaped [`serde_json::Value`].
    /// Sibling of [`Self::export_ledger`]; powers `burn stamps export`.
    pub fn export_stamps(
        &self,
        _opts: ExportStampsOptions,
    ) -> anyhow::Result<impl Iterator<Item = serde_json::Value>> {
        let mut buf: Vec<u8> = Vec::new();
        self.inner.export_stamps_jsonl(&mut buf)?;
        Ok(parse_jsonl_lines(&buf)?.into_iter())
    }
}

/// Free-function form of [`LedgerHandle::export_stamps`].
pub fn export_stamps(
    opts: ExportStampsOptions,
) -> anyhow::Result<impl Iterator<Item = serde_json::Value>> {
    let handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        content_home: None,
    })?;
    let mut buf: Vec<u8> = Vec::new();
    handle.inner.export_stamps_jsonl(&mut buf)?;
    Ok(parse_jsonl_lines(&buf)?.into_iter())
}

// --- helpers ---------------------------------------------------------------

/// Split a JSONL byte buffer (as produced by the lower-crate `_jsonl`
/// writers) into one [`serde_json::Value`] per non-empty line.
fn parse_jsonl_lines(buf: &[u8]) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for line in buf.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        out.push(serde_json::from_slice(line)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentKind, ContentRecord, ContentRole, SourceKind};

    fn make_content(session: &str, message: &str, text: &str) -> ContentRecord {
        ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.into(),
            message_id: message.into(),
            ts: "2025-01-01T00:00:00Z".into(),
            role: ContentRole::Assistant,
            kind: ContentKind::Text,
            text: Some(text.into()),
            tool_use: None,
            tool_result: None,
        }
    }

    fn open_handle(tmp: &tempfile::TempDir) -> LedgerHandle {
        Ledger::open(LedgerOpenOptions::with_home(tmp.path())).unwrap()
    }

    #[test]
    fn search_finds_known_token_in_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut handle = open_handle(&tmp);
        handle
            .raw_mut()
            .append_content(&[
                make_content("ses_a", "m1", "the build failed with an out of memory error"),
                make_content("ses_a", "m2", "permission denied while reading file"),
            ])
            .unwrap();

        let result = handle
            .search(SearchQueryOptions::new("memory"))
            .unwrap();
        assert_eq!(result.query, "memory");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].session_id, "ses_a");
        assert_eq!(result.hits[0].message_id, "m1");
        assert!(result.hits[0].snippet.contains("<b>"));
    }

    #[test]
    fn search_respects_limit_and_session_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut handle = open_handle(&tmp);
        handle
            .raw_mut()
            .append_content(&[
                make_content("ses_a", "m1", "needle in a haystack"),
                make_content("ses_b", "m1", "needle on a beach"),
                make_content("ses_b", "m2", "needle in a stack"),
            ])
            .unwrap();

        let scoped = handle
            .search(SearchQueryOptions {
                query: "needle".into(),
                limit: Some(10),
                session_id: Some("ses_a".into()),
                ledger_home: None,
            })
            .unwrap();
        assert_eq!(scoped.hits.len(), 1);
        assert_eq!(scoped.hits[0].session_id, "ses_a");

        let capped = handle
            .search(SearchQueryOptions {
                query: "needle".into(),
                limit: Some(1),
                session_id: None,
                ledger_home: None,
            })
            .unwrap();
        assert_eq!(capped.hits.len(), 1);
    }

    #[test]
    fn search_free_function_opens_its_own_ledger() {
        let tmp = tempfile::TempDir::new().unwrap();
        {
            let mut handle = open_handle(&tmp);
            handle
                .raw_mut()
                .append_content(&[make_content("ses_a", "m1", "haystack with needle")])
                .unwrap();
        }
        let result = search(SearchQueryOptions {
            query: "needle".into(),
            limit: None,
            session_id: None,
            ledger_home: Some(tmp.path().to_path_buf()),
        })
        .unwrap();
        assert_eq!(result.hits.len(), 1);
    }

    #[test]
    fn export_ledger_returns_one_value_per_record() {
        use relayburn_reader::{TurnRecord, Usage};

        let tmp = tempfile::TempDir::new().unwrap();
        let mut handle = open_handle(&tmp);
        let turn = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "ses_a".into(),
            session_path: None,
            message_id: "m1".into(),
            turn_index: 0,
            ts: "2025-01-01T00:00:00Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            usage: Usage::default(),
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        };
        handle.raw_mut().append_turns(&[turn]).unwrap();

        let values: Vec<_> = handle
            .export_ledger(ExportLedgerOptions::default())
            .unwrap()
            .collect();
        assert!(!values.is_empty(), "expected at least one exported record");
        let first = &values[0];
        assert_eq!(first["v"], 1);
        assert_eq!(first["kind"], "turn");
        assert_eq!(first["record"]["sessionId"], "ses_a");
        assert_eq!(first["record"]["messageId"], "m1");
    }

    #[test]
    fn export_ledger_free_function_opens_its_own_ledger() {
        let tmp = tempfile::TempDir::new().unwrap();
        {
            let mut handle = open_handle(&tmp);
            handle
                .raw_mut()
                .append_content(&[make_content("ses_a", "m1", "anything")])
                .unwrap();
        }
        // No turns appended → expect zero values, but the call must succeed.
        let values: Vec<_> = export_ledger(ExportLedgerOptions {
            ledger_home: Some(tmp.path().to_path_buf()),
        })
        .unwrap()
        .collect();
        assert!(values.is_empty());
    }

    #[test]
    fn export_stamps_returns_appended_stamps() {
        use relayburn_ledger::{Stamp, StampSelector};
        use std::collections::BTreeMap;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut handle = open_handle(&tmp);

        let mut enrichment = BTreeMap::new();
        enrichment.insert("role".to_string(), "fix-bug".to_string());
        let stamp = Stamp::new(
            "2025-01-01T00:00:00Z",
            StampSelector {
                session_id: Some("ses_a".into()),
                message_id: None,
                range: None,
            },
            enrichment,
        )
        .unwrap();
        handle.raw_mut().append_stamp(&stamp).unwrap();

        let values: Vec<_> = handle
            .export_stamps(ExportStampsOptions::default())
            .unwrap()
            .collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["v"], 1);
        assert_eq!(values[0]["kind"], "stamp");
        assert_eq!(values[0]["enrichment"]["role"], "fix-bug");
    }
}
