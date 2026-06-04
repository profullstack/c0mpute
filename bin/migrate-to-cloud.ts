#!/usr/bin/env bun
/**
 * Migrate blog_posts from local SQLite (better-sqlite3) to SQLite Cloud.
 *
 * Usage:
 *   bun bin/migrate-to-cloud.ts
 *   bun bin/migrate-to-cloud.ts --db /path/to/blog.db   # custom local path
 *   bun bin/migrate-to-cloud.ts --dry-run               # print rows, skip upload
 *
 * Requires SQLITE_CLOUD_URL in .env (already set from c0mpute setup).
 * Local DB path defaults to BLOG_DB_PATH env var or ./blog.db.
 */

import { parseArgs } from "node:util";
import { existsSync, readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// Load .env manually (Bun auto-loads but the Cloud URL may not be set in shell)
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
  console.log("Usage: bun bin/migrate-to-cloud.ts [--db <path>] [--dry-run]");
  process.exit(0);
}

const LOCAL_PATH = values.db ?? process.env.BLOG_DB_PATH ?? resolve(ROOT, "blog.db");

if (!existsSync(LOCAL_PATH)) {
  console.error(`Local DB not found: ${LOCAL_PATH}`);
  process.exit(1);
}

const CLOUD_URL = process.env.SQLITE_CLOUD_URL;
if (!CLOUD_URL) {
  console.error("SQLITE_CLOUD_URL is not set in .env");
  process.exit(1);
}

// ── Read local posts ──────────────────────────────────────────────────────────

// @ts-ignore — bun resolves this at runtime
const BetterSQLite = (await import("better-sqlite3")).default;
const local = new BetterSQLite(LOCAL_PATH, { readonly: true });

interface LocalRow {
  id: number;
  source: string;
  source_id: string;
  slug: string;
  title: string;
  content_html: string;
  content_markdown: string | null;
  meta_description: string | null;
  image_url: string | null;
  tags: string;
  published_at: string;
  created_at: string;
}

const rows = local.prepare("SELECT * FROM blog_posts ORDER BY published_at ASC").all() as LocalRow[];
local.close();

console.log(`Found ${rows.length} post(s) in local DB: ${LOCAL_PATH}`);

if (rows.length === 0) {
  console.log("Nothing to migrate.");
  process.exit(0);
}

if (values["dry-run"]) {
  console.log("\nDry run — posts that would be migrated:");
  for (const r of rows) console.log(`  [${r.published_at.slice(0, 10)}] ${r.slug}`);
  process.exit(0);
}

// ── Upsert into SQLite Cloud ──────────────────────────────────────────────────

// @ts-ignore
const { Database } = await import("@sqlitecloud/drivers");
const cloud = new Database(CLOUD_URL);

// Ensure table exists
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

let migrated = 0;
let skipped = 0;
let failed = 0;

for (const row of rows) {
  try {
    await cloud.sql`
      INSERT INTO blog_posts
        (source, source_id, slug, title, content_html, content_markdown,
         meta_description, image_url, tags, published_at)
      VALUES (
        ${row.source}, ${row.source_id}, ${row.slug}, ${row.title},
        ${row.content_html}, ${row.content_markdown ?? null},
        ${row.meta_description ?? null}, ${row.image_url ?? null},
        ${row.tags}, ${row.published_at}
      )
      ON CONFLICT(source, source_id) DO UPDATE SET
        slug             = excluded.slug,
        title            = excluded.title,
        content_html     = excluded.content_html,
        content_markdown = excluded.content_markdown,
        meta_description = excluded.meta_description,
        image_url        = excluded.image_url,
        tags             = excluded.tags,
        published_at     = excluded.published_at
    `;
    console.log(`  ✓ ${row.slug}`);
    migrated++;
  } catch (err) {
    console.error(`  ✗ ${row.slug} — ${err instanceof Error ? err.message : err}`);
    failed++;
  }
}

await cloud.close?.();

console.log(`\nDone. migrated=${migrated} skipped=${skipped} failed=${failed}`);
if (failed > 0) process.exit(1);
