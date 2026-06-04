#!/usr/bin/env bun
/**
 * Export blog_posts from SQLite Cloud → local blog.db (for drizzle-kit studio).
 *
 * Usage:
 *   node --experimental-strip-types bin/export-from-cloud.ts
 *   pnpm export:cloud
 *   pnpm export:cloud -- --dry-run
 *   pnpm export:cloud -- --db /path/to/output.db
 *
 * Requires SQLITE_CLOUD_URL in .env or environment.
 */

import { parseArgs } from "node:util";
import { existsSync, readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const envPath = resolve(ROOT, ".env");
if (existsSync(envPath)) {
  for (const line of readFileSync(envPath, "utf8").split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const eq = t.indexOf("=");
    if (eq === -1) continue;
    const k = t.slice(0, eq).trim();
    const v = t.slice(eq + 1).trim();
    if (k && !(k in process.env)) process.env[k] = v;
  }
}

const { values } = parseArgs({
  args: process.argv.slice(2),
  options: {
    db:        { type: "string" },
    "dry-run": { type: "boolean", default: false },
    help:      { type: "boolean", default: false },
  },
});

if (values.help) {
  console.log("Usage: pnpm export:cloud [-- --db <path>] [-- --dry-run]");
  process.exit(0);
}

const CLOUD_URL = process.env.SQLITE_CLOUD_URL;
if (!CLOUD_URL) {
  console.error("SQLITE_CLOUD_URL is not set in .env");
  process.exit(1);
}

const LOCAL_PATH = values.db ?? process.env.BLOG_DB_PATH ?? resolve(ROOT, "blog.db");

// ── Fetch from cloud ──────────────────────────────────────────────────────────

const { Database } = await import("@sqlitecloud/drivers");
const cloud = new Database(CLOUD_URL);

// Ensure table exists (creates it if this is a fresh DB)
await cloud.sql`
  CREATE TABLE IF NOT EXISTS blog_posts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    source           TEXT NOT NULL DEFAULT 'crawlproof',
    source_id        TEXT NOT NULL,
    slug             TEXT NOT NULL,
    title            TEXT NOT NULL,
    content_html     TEXT NOT NULL,
    content_markdown TEXT,
    meta_description TEXT,
    image_url        TEXT,
    tags             TEXT NOT NULL DEFAULT '[]',
    published_at     TEXT NOT NULL,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    UNIQUE(source, source_id)
  )
`;
await cloud.sql`CREATE INDEX IF NOT EXISTS idx_posts_slug      ON blog_posts(slug)`;
await cloud.sql`CREATE INDEX IF NOT EXISTS idx_posts_published ON blog_posts(published_at DESC)`;

const rows = (await cloud.sql`
  SELECT * FROM blog_posts ORDER BY published_at ASC
`) as Record<string, unknown>[];

await cloud.close?.();

console.log(`Fetched ${rows.length} post(s) from SQLite Cloud`);

if (rows.length === 0) {
  console.log("Nothing to export.");
  process.exit(0);
}

if (values["dry-run"]) {
  console.log("\nDry run — posts that would be written:");
  for (const r of rows) console.log(`  [${String(r.published_at).slice(0, 10)}] ${r.slug}`);
  process.exit(0);
}

// ── Write to local SQLite ─────────────────────────────────────────────────────

// @ts-ignore
const BetterSQLite = (await import("better-sqlite3")).default;
const local = new BetterSQLite(LOCAL_PATH);

local.exec(`
  CREATE TABLE IF NOT EXISTS blog_posts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    source           TEXT NOT NULL DEFAULT 'crawlproof',
    source_id        TEXT NOT NULL,
    slug             TEXT NOT NULL,
    title            TEXT NOT NULL,
    content_html     TEXT NOT NULL,
    content_markdown TEXT,
    meta_description TEXT,
    image_url        TEXT,
    tags             TEXT NOT NULL DEFAULT '[]',
    published_at     TEXT NOT NULL,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    UNIQUE(source, source_id)
  )
`);

const upsert = local.prepare(`
  INSERT INTO blog_posts
    (source, source_id, slug, title, content_html, content_markdown,
     meta_description, image_url, tags, published_at, created_at)
  VALUES
    (@source, @source_id, @slug, @title, @content_html, @content_markdown,
     @meta_description, @image_url, @tags, @published_at, @created_at)
  ON CONFLICT(source, source_id) DO UPDATE SET
    slug             = excluded.slug,
    title            = excluded.title,
    content_html     = excluded.content_html,
    content_markdown = excluded.content_markdown,
    meta_description = excluded.meta_description,
    image_url        = excluded.image_url,
    tags             = excluded.tags,
    published_at     = excluded.published_at
`);

const insertMany = local.transaction((posts: Record<string, unknown>[]) => {
  for (const row of posts) upsert.run(row);
});

insertMany(rows);
local.close();

console.log(`Exported ${rows.length} post(s) to ${LOCAL_PATH}`);
console.log("Run: pnpm drizzle-kit studio");
