import { describe, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { localSql } from "./local";

describe("localSql", () => {
  const path = join(mkdtempSync(join(tmpdir(), "c0db-")), "site.db");
  const sql = localSql(path);
  test("runs multi-statement DDL, binds params, undefined as NULL", async () => {
    await sql.run(`CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER, note TEXT); CREATE INDEX IF NOT EXISTS t_n ON t (n)`);
    await sql.run(`INSERT INTO t (id, n, note) VALUES (?, ?, ?)`, ["a", 1, undefined]);
    await sql.run(`INSERT OR REPLACE INTO t (id, n, note) VALUES (?, ?, ?)`, ["a", 2, "x"]);
    expect(await sql.all(`SELECT id, n, note FROM t WHERE id = ?`, ["a"])).toEqual([{ id: "a", n: 2, note: "x" }]);
    const idx = await sql.all(`SELECT name FROM sqlite_master WHERE type = 'index' AND name = 't_n'`);
    expect(idx.length).toBe(1);
  });
  test("a primary-key clash is an error, like the cloud driver", async () => {
    await expect(sql.run(`INSERT INTO t (id, n) VALUES (?, ?)`, ["a", 3])).rejects.toThrow();
  });
  test("WAL mode is on", async () => {
    expect(await sql.all(`PRAGMA journal_mode`)).toEqual([{ journal_mode: "wal" }]);
  });
});
