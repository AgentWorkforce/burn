// Initial schema for SqliteAdapter. SQL is inlined as a TS string literal
// (rather than a `.sql` sidecar) so it travels through `tsc --build` into
// `dist/` without needing an explicit copy step in the build / publish path.
//
// The schema is dialect-templated: substitutions are flagged with `{{name}}`
// placeholders that `applyDialect()` rewrites for each backend. Today only
// `sqlite` is wired up; the Postgres adapter (Phase 3 of #139) will swap
// `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGSERIAL PRIMARY KEY`,
// `INSERT OR IGNORE` → `INSERT … ON CONFLICT DO NOTHING`, etc.
//
// Tables in this migration:
//
//   `turns`, `compactions`, `relationships`, `tool_result_events`,
//   `user_turns` — one row per ledger record, primary keyed on the canonical
//   dedup hash from `index-sidecar.ts`. `INSERT OR IGNORE` collapses
//   duplicates natively, replacing the JSONL adapter's separate `.idx`
//   sidecar.
//
//   `stamps` — append-only enrichment ledger. Folded into turns at query
//   time via `stampMatches`.
//
//   `content` — per-message content records. Unique on
//   `(session_id, content_fp)` so re-ingest is idempotent without a
//   per-session lock.
//
//   `locks` — cross-process named locks. `withLock(name, fn)` inserts a row
//   under `BEGIN IMMEDIATE` so a second writer on the same DB file blocks
//   until the first releases the row.
//
//   `schema_state` — single-row table tracking applied migration version.

export const VERSION = 1;

export const SQL_SQLITE = `
CREATE TABLE IF NOT EXISTS schema_state (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);

INSERT OR IGNORE INTO schema_state (id, version) VALUES (1, 1);

CREATE TABLE IF NOT EXISTS locks (
  name           TEXT PRIMARY KEY,
  acquired_at_ms INTEGER NOT NULL,
  pid            INTEGER
);

CREATE TABLE IF NOT EXISTS turns (
  id_hash      TEXT PRIMARY KEY,
  content_fp   TEXT NOT NULL UNIQUE,
  source       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  message_id   TEXT NOT NULL,
  turn_index   INTEGER NOT NULL,
  ts           TEXT NOT NULL,
  project      TEXT,
  project_key  TEXT,
  record_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(source, session_id);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);
CREATE INDEX IF NOT EXISTS idx_turns_session_index ON turns(source, session_id, turn_index);
CREATE INDEX IF NOT EXISTS idx_turns_project_key ON turns(project_key);

CREATE TABLE IF NOT EXISTS compactions (
  id_hash      TEXT PRIMARY KEY,
  source       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  ts           TEXT NOT NULL,
  record_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_compactions_session ON compactions(source, session_id);
CREATE INDEX IF NOT EXISTS idx_compactions_ts ON compactions(ts);

CREATE TABLE IF NOT EXISTS relationships (
  id_hash             TEXT PRIMARY KEY,
  source              TEXT NOT NULL,
  session_id          TEXT NOT NULL,
  related_session_id  TEXT,
  ts                  TEXT,
  record_json         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_relationships_session ON relationships(source, session_id);
CREATE INDEX IF NOT EXISTS idx_relationships_related ON relationships(related_session_id);

CREATE TABLE IF NOT EXISTS tool_result_events (
  id_hash      TEXT PRIMARY KEY,
  source       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  ts           TEXT,
  record_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tool_result_events_session ON tool_result_events(source, session_id);
CREATE INDEX IF NOT EXISTS idx_tool_result_events_ts ON tool_result_events(ts);

CREATE TABLE IF NOT EXISTS user_turns (
  id_hash      TEXT PRIMARY KEY,
  source       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  ts           TEXT NOT NULL,
  record_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_user_turns_session ON user_turns(source, session_id);
CREATE INDEX IF NOT EXISTS idx_user_turns_ts ON user_turns(ts);

CREATE TABLE IF NOT EXISTS stamps (
  seq             INTEGER PRIMARY KEY AUTOINCREMENT,
  ts              TEXT NOT NULL,
  selector_json   TEXT NOT NULL,
  enrichment_json TEXT NOT NULL,
  session_id      TEXT,
  message_id      TEXT
);

CREATE INDEX IF NOT EXISTS idx_stamps_session ON stamps(session_id);
CREATE INDEX IF NOT EXISTS idx_stamps_message ON stamps(message_id);
CREATE INDEX IF NOT EXISTS idx_stamps_ts ON stamps(ts);

CREATE TABLE IF NOT EXISTS content (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  source       TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  message_id   TEXT NOT NULL,
  ts           TEXT NOT NULL,
  content_fp   TEXT NOT NULL,
  record_json  TEXT NOT NULL,
  mtime_ms     INTEGER NOT NULL,
  UNIQUE (session_id, content_fp)
);

CREATE INDEX IF NOT EXISTS idx_content_session ON content(session_id);
CREATE INDEX IF NOT EXISTS idx_content_session_message ON content(session_id, message_id);
CREATE INDEX IF NOT EXISTS idx_content_mtime ON content(mtime_ms);
`;
