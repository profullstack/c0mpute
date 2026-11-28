// The site's database as a local SQLite file (DATABASE_PATH), self-hosted on
// the box that serves c0mpute.com: no third-party database, no DB port, no
// network hop that can drop ("SQLiteCloudError: Server ended the connection").
//
// `next start` runs on Bun in our image, so bun:sqlite is there. It is loaded
// through createRequire so the Next bundler never tries to resolve it.
import { createRequire } from "node:module";
import type { Sql } from "../chat/store";

type BunStatement = { all(...p: unknown[]): Record<string, unknown>[]; run(...p: unknown[]): unknown };
type BunDatabase = { query(q: string): BunStatement; exec(q: string): void; close(): void };

const handles = new Map<string, BunDatabase>();

/** Open (once per path) a WAL-mode SQLite file. */
export function openLocal(path: string): BunDatabase {
  const have = handles.get(path);
  if (have) return have;
  const { Database } = createRequire(import.meta.url)("bun:sqlite") as { Database: new (p: string, o?: { create?: boolean }) => BunDatabase };
  const db = new Database(path, { create: true });
  db.exec("PRAGMA journal_mode = WAL");
  db.exec("PRAGMA synchronous = NORMAL");
  // A deploy overlaps the old and new process for a moment: wait, don't fail.
  db.exec("PRAGMA busy_timeout = 10000");
  db.exec("PRAGMA foreign_keys = OFF");
  handles.set(path, db);
  return db;
}

/** The Sql shape every store takes, over a local file. */
export function localSql(path: string): Sql {
  // bun:sqlite refuses `undefined`; the cloud driver bound it as NULL.
  const bind = (p: unknown[] = []) => p.map((v) => (v === undefined ? null : v));
  return {
    all: async (q, p = []) => openLocal(path).query(q).all(...bind(p as unknown[])).map((row) => ({ ...row })),
    run: async (q, p = []) => {
      // No parameters: exec, which runs every statement in the string (the
      // cloud driver did; query() compiles only the first).
      if (!p.length) return void openLocal(path).exec(q);
      openLocal(path).query(q).run(...bind(p as unknown[]));
    },
  };
}
