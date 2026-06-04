import { Database } from "@sqlitecloud/drivers";
import Redis from "ioredis";

const CACHE_TTL = 30 * 60; // 30 minutes in seconds

// ── SQLite Cloud ──────────────────────────────────────────────────────────────

let _db: Database | null = null;

async function getDb(): Promise<Database> {
  if (_db) return _db;
  const url = process.env.SQLITE_CLOUD_URL;
  if (!url) throw new Error("SQLITE_CLOUD_URL env var is required");
  const db = new Database(url);
  await db.sql`
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
  await db.sql`CREATE INDEX IF NOT EXISTS idx_posts_slug      ON blog_posts(slug)`;
  await db.sql`CREATE INDEX IF NOT EXISTS idx_posts_published ON blog_posts(published_at DESC)`;
  _db = db;
  return _db;
}

// ── Redis cache ───────────────────────────────────────────────────────────────

let _redis: Redis | null = null;

function getRedis(): Redis | null {
  if (_redis) return _redis;
  const url = process.env.REDIS_URL;
  if (!url) return null;
  _redis = new Redis(url, { lazyConnect: true, maxRetriesPerRequest: 1 });
  _redis.on("error", () => {}); // prevent unhandled rejections when Redis is unreachable
  return _redis;
}

async function cacheGet<T>(key: string): Promise<T | null> {
  try {
    const redis = getRedis();
    if (!redis) return null;
    const raw = await redis.get(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

async function cacheSet(key: string, value: unknown): Promise<void> {
  try {
    const redis = getRedis();
    if (!redis) return;
    await redis.set(key, JSON.stringify(value), "EX", CACHE_TTL);
  } catch {
    // non-fatal — fall through to the live DB
  }
}

async function cacheDel(...keys: string[]): Promise<void> {
  try {
    const redis = getRedis();
    if (!redis) return;
    await redis.del(...keys);
  } catch {
    // non-fatal
  }
}

// Deletes all list cache keys (pattern scan). Used after a write.
async function cacheDelListKeys(): Promise<void> {
  try {
    const redis = getRedis();
    if (!redis) return;
    let cursor = "0";
    do {
      const [next, keys] = await redis.scan(cursor, "MATCH", "blog:list:*", "COUNT", 100);
      cursor = next;
      if (keys.length > 0) await redis.del(...keys);
    } while (cursor !== "0");
  } catch {
    // non-fatal
  }
}

// ── Public types & helpers ────────────────────────────────────────────────────

export interface BlogPost {
  id: number;
  source: string;
  source_id: string;
  slug: string;
  title: string;
  content_html: string;
  content_markdown: string | null;
  meta_description: string | null;
  image_url: string | null;
  tags: string[];
  published_at: string;
  created_at: string;
}

type NewPost = Omit<BlogPost, "id" | "created_at">;

function hydrate(row: Record<string, unknown>): BlogPost {
  return { ...row, tags: JSON.parse((row.tags as string) ?? "[]") } as BlogPost;
}

// ── Exported functions ────────────────────────────────────────────────────────

export async function upsertPost(post: NewPost): Promise<void> {
  const db = await getDb();
  const tagsJson = JSON.stringify(post.tags);
  await db.sql`
    INSERT INTO blog_posts
      (source, source_id, slug, title, content_html, content_markdown,
       meta_description, image_url, tags, published_at)
    VALUES (
      ${post.source}, ${post.source_id}, ${post.slug}, ${post.title},
      ${post.content_html}, ${post.content_markdown ?? null},
      ${post.meta_description ?? null}, ${post.image_url ?? null},
      ${tagsJson}, ${post.published_at}
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
  // Invalidate: this post + all list pages
  await Promise.all([
    cacheDel(`blog:post:${post.slug}`),
    cacheDelListKeys(),
  ]);
}

export async function listPosts(limit = 50, offset = 0): Promise<BlogPost[]> {
  const cacheKey = `blog:list:${limit}:${offset}`;
  const cached = await cacheGet<BlogPost[]>(cacheKey);
  if (cached) return cached;

  const db = await getDb();
  const rows = (await db.sql`
    SELECT * FROM blog_posts ORDER BY published_at DESC LIMIT ${limit} OFFSET ${offset}
  `) as Record<string, unknown>[];
  const posts = (rows ?? []).map(hydrate);
  await cacheSet(cacheKey, posts);
  return posts;
}

export async function getPost(slug: string): Promise<BlogPost | null> {
  const cacheKey = `blog:post:${slug}`;
  const cached = await cacheGet<BlogPost>(cacheKey);
  if (cached) return cached;

  const db = await getDb();
  const rows = (await db.sql`
    SELECT * FROM blog_posts WHERE slug = ${slug} LIMIT 1
  `) as Record<string, unknown>[];
  const row = rows?.[0] ?? null;
  const post = row ? hydrate(row) : null;
  if (post) await cacheSet(cacheKey, post);
  return post;
}
