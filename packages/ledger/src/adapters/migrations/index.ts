// Ordered list of schema migrations applied at adapter open. Each entry
// declares its dialect-specific SQL so future backends (Postgres) can
// substitute their own statements without duplicating the migration list.
//
// Adding a migration:
//   1. Drop a new `NNN_<name>.ts` next to this file exporting `SQL_SQLITE`
//      (and `SQL_POSTGRES` once Phase 3 lands).
//   2. Append it to `MIGRATIONS` below.
//   3. Bump the per-migration `VERSION` constant. The adapter advances the
//      `schema_state.version` row to `MIGRATIONS.at(-1).version` after
//      applying everything past the current on-disk version.

import * as initial from './001_initial.js';

export type Dialect = 'sqlite' | 'postgres';

export interface Migration {
  version: number;
  sql: Record<Dialect, string | undefined>;
}

export const MIGRATIONS: Migration[] = [
  {
    version: initial.VERSION,
    sql: {
      sqlite: initial.SQL_SQLITE,
      // Postgres dialect lands with Phase 3 (#142). Until then the factory
      // refuses RELAYBURN_STORAGE=postgres, so this gap can't be hit at
      // runtime.
      postgres: undefined,
    },
  },
];

// Highest version any migration declares. Adapters compare this to the
// `schema_state.version` row to decide whether the on-disk DB is current.
export const LATEST_VERSION = MIGRATIONS.reduce(
  (max, m) => (m.version > max ? m.version : max),
  0,
);
