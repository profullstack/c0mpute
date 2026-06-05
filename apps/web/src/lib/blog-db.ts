import { Database } from "@sqlitecloud/drivers";
import { drizzle } from "drizzle-orm/sqlite-proxy";
import { desc, eq } from "drizzle-orm";
import Redis from "ioredis";
import { blogPosts } from "./schema";

// ── SQLite Cloud + Drizzle ────────────────────────────────────────────────────

type Db = ReturnType<typeof drizzle<{ blogPosts: typeof blogPosts }>>;

let _client: Database | null = null;
let _db: Db | null = null;

const CONNECTION_ERRORS = new Set([
  "ERR_CONNECTION_NOT_ESTABLISHED",
  "ERR_SOCKET_CONNECTION_TIMEOUT",
  "ERR_CONNECTION_CLOSED",
]);

function isConnectionError(e: unknown): boolean {
  const code =
    (e as { errorCode?: string })?.errorCode ??
    (e as { cause?: { errorCode?: string } })?.cause?.errorCode;
  return !!code && CONNECTION_ERRORS.has(code);
}

function resetConnection() {
  try {
    (_client as Database & { disconnect?: () => void })?.disconnect?.();
  } catch {}
  _client = null;
  _db = null;
}

async function makeConnection(): Promise<Db> {
  const url = process.env.SQLITE_CLOUD_URL;
  if (!url) throw new Error("SQLITE_CLOUD_URL env var is required");

  const client = new Database(url);

  // Ensure table + indexes exist (drizzle-kit migrations not yet wired to cloud)
  await client.sql`
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
  await client.sql`CREATE INDEX IF NOT EXISTS idx_posts_slug      ON blog_posts(slug)`;
  await client.sql`CREATE INDEX IF NOT EXISTS idx_posts_published ON blog_posts(published_at DESC)`;

  _client = client;
  _db = drizzle(
    async (sql, params, method) => {
      if (method === "run") {
        await client.sql({ query: sql, parameters: params });
        return { rows: [] };
      }
      const result = await client.sql({ query: sql, parameters: params });
      const raw = Array.isArray(result) ? result : result ? [result] : [];
      const rows = raw.map((row: Record<string, unknown>) => Object.values(row));
      return { rows };
    },
    { schema: { blogPosts } }
  );

  return _db;
}

async function getDb(): Promise<Db> {
  if (_db) return _db;
  return makeConnection();
}

// Runs fn; on SQLiteCloud connection error resets the singleton and retries once.
async function withReconnect<T>(fn: () => Promise<T>): Promise<T> {
  try {
    return await fn();
  } catch (e) {
    if (isConnectionError(e)) {
      resetConnection();
      return fn();
    }
    throw e;
  }
}

// ── Redis cache ───────────────────────────────────────────────────────────────

let _redis: Redis | null = null;

function getRedis(): Redis | null {
  if (_redis) return _redis;
  const url = process.env.REDIS_URL;
  if (!url) return null;
  _redis = new Redis(url, { lazyConnect: true, maxRetriesPerRequest: 1 });
  _redis.on("error", () => {});
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
  } catch {}
}

async function cacheDel(...keys: string[]): Promise<void> {
  try {
    const redis = getRedis();
    if (!redis) return;
    await redis.del(...keys);
  } catch {}
}

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
  } catch {}
}

const CACHE_TTL = 30 * 60;

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

function hydrate(row: typeof blogPosts.$inferSelect): BlogPost {
  return { ...row, tags: JSON.parse((row.tags as string) ?? "[]") };
}

// ── Exported functions ────────────────────────────────────────────────────────

export async function upsertPost(post: NewPost): Promise<void> {
  const tagsJson = JSON.stringify(post.tags);
  await withReconnect(async () => {
    const db = await getDb();
    await db
      .insert(blogPosts)
      .values({ ...post, tags: tagsJson })
      .onConflictDoUpdate({
        target: [blogPosts.source, blogPosts.source_id],
        set: {
          slug: post.slug,
          title: post.title,
          content_html: post.content_html,
          content_markdown: post.content_markdown ?? null,
          meta_description: post.meta_description ?? null,
          image_url: post.image_url ?? null,
          tags: tagsJson,
          published_at: post.published_at,
        },
      });
  });
  await Promise.all([cacheDel(`blog:post:${post.slug}`), cacheDelListKeys()]);
}

export async function listPosts(limit = 50, offset = 0): Promise<BlogPost[]> {
  const cacheKey = `blog:list:${limit}:${offset}`;
  const cached = await cacheGet<BlogPost[]>(cacheKey);
  if (cached) return cached;

  const posts = await withReconnect(async () => {
    const db = await getDb();
    const rows = await db
      .select()
      .from(blogPosts)
      .orderBy(desc(blogPosts.published_at))
      .limit(limit)
      .offset(offset);
    return rows.map(hydrate);
  });

  await cacheSet(cacheKey, posts);
  return posts;
}

export async function getPost(slug: string): Promise<BlogPost | null> {
  const cacheKey = `blog:post:${slug}`;
  const cached = await cacheGet<BlogPost>(cacheKey);
  if (cached) return cached;

  const post = await withReconnect(async () => {
    const db = await getDb();
    const rows = await db
      .select()
      .from(blogPosts)
      .where(eq(blogPosts.slug, slug))
      .limit(1);
    return rows[0] ? hydrate(rows[0]) : null;
  });

  if (post) await cacheSet(cacheKey, post);
  return post;
}
