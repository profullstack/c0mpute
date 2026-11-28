#!/usr/bin/env bun
// One-shot copy of the site database from SQLite Cloud to a self-hosted local
// SQLite file, for the DATABASE_PATH cutover (2026-11-28: no third-party DB).
//
//   SQLITE_CLOUD_URL=sqlitecloud://... bun bin/cloud-to-local.ts /data/c0mpute.db
//
// Copies every table's DDL, rows and indexes, then checks each row count.
// Refuses to write into a file that already has tables (no merge, no clobber).
// Run it with the app stopped (or right after stopping it) so no write is lost.
import { Database as Cloud } from "@sqlitecloud/drivers";
import { Database as Local } from "bun:sqlite";

const url = process.env.SQLITE_CLOUD_URL;
const out = process.argv[2];
if (!url || !out) {
  console.error("usage: SQLITE_CLOUD_URL=... bun bin/cloud-to-local.ts <path/to/local.db>");
  process.exit(2);
}
// Internal tables of the old host, never used by the app.
const SKIP = /^(sqlite_|_litestream_|_sqliteai_)/;
const BATCH = 500;

const cloud = new Cloud(url);
const local = new Local(out, { create: true });
local.exec("PRAGMA journal_mode = WAL");
local.exec("PRAGMA foreign_keys = OFF");

const existing = local.query(`SELECT count(*) AS n FROM sqlite_master WHERE type = 'table'`).get() as { n: number };
if (existing.n > 0) {
  console.error(`${out} already has ${existing.n} tables; refusing to merge into it`);
  process.exit(1);
}

type Obj = { type: string; name: string; tbl_name: string; sql: string | null };
const objs = ((await cloud.sql`SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type = 'table' DESC, name`) as Obj[]).filter(
  (o) => !SKIP.test(o.name) && !SKIP.test(o.tbl_name),
);
const tables = objs.filter((o) => o.type === "table");
let total = 0;
for (const t of tables) {
  local.exec(t.sql!);
  const q = `"${t.name.replace(/"/g, '""')}"`;
  let copied = 0;
  for (let offset = 0; ; offset += BATCH) {
    const rows = (await cloud.sql({ query: `SELECT * FROM ${q} ORDER BY rowid LIMIT ? OFFSET ?`, parameters: [BATCH, offset] })) as Record<string, unknown>[];
    if (!rows.length) break;
    const cols = Object.keys(rows[0]);
    const ins = local.prepare(`INSERT INTO ${q} (${cols.map((c) => `"${c.replace(/"/g, '""')}"`).join(", ")}) VALUES (${cols.map(() => "?").join(", ")})`);
    local.transaction((rs: Record<string, unknown>[]) => {
      for (const r of rs) ins.run(...cols.map((c) => (r[c] === undefined ? null : (r[c] as never))));
    })(rows);
    copied += rows.length;
    if (rows.length < BATCH) break;
  }
  const [{ n }] = (await cloud.sql({ query: `SELECT count(*) AS n FROM ${q}`, parameters: [] })) as { n: number }[];
  const mine = (local.query(`SELECT count(*) AS n FROM ${q}`).get() as { n: number }).n;
  if (Number(n) !== mine) {
    console.error(`MISMATCH ${t.name}: cloud ${n}, local ${mine}`);
    process.exit(1);
  }
  total += mine;
  console.log(`${t.name}: ${mine}`);
}
for (const o of objs.filter((o) => o.type !== "table")) local.exec(o.sql!);
local.exec("PRAGMA wal_checkpoint(TRUNCATE)");
console.log(`done: ${tables.length} tables, ${total} rows -> ${out}`);
local.close();
cloud.close();
