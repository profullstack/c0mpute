import { Database } from "bun:sqlite";
import { resolve } from "node:path";

// Default to ./blog.db; set BLOG_DB_PATH to a Railway volume mount for persistence.
const DB_PATH = process.env.BLOG_DB_PATH ?? resolve(process.cwd(), "blog.db");

let _db: InstanceType<typeof Database> | null = null;

function getDb(): InstanceType<typeof Database> {
  if (!_db) {
    _db = new Database(DB_PATH, { create: true });
    _db.exec(`
      CREATE TABLE IF NOT EXISTS blog_posts (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        source         TEXT NOT NULL DEFAULT 'crawlproof',
        source_id      TEXT NOT NULL,
        slug           TEXT NOT NULL,
        title          TEXT NOT NULL,
        content_html   TEXT NOT NULL,
        content_markdown TEXT,
        meta_description TEXT,
        image_url      TEXT,
        tags           TEXT NOT NULL DEFAULT '[]',
        published_at   TEXT NOT NULL,
        created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
        UNIQUE(source, source_id)
      );
      CREATE INDEX IF NOT EXISTS idx_posts_slug      ON blog_posts(slug);
      CREATE INDEX IF NOT EXISTS idx_posts_published ON blog_posts(published_at DESC);
    `);
  }
  return _db;
}

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

export function upsertPost(post: NewPost): void {
  getDb().prepare(`
    INSERT INTO blog_posts
      (source, source_id, slug, title, content_html, content_markdown,
       meta_description, image_url, tags, published_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(source, source_id) DO UPDATE SET
      slug             = excluded.slug,
      title            = excluded.title,
      content_html     = excluded.content_html,
      content_markdown = excluded.content_markdown,
      meta_description = excluded.meta_description,
      image_url        = excluded.image_url,
      tags             = excluded.tags,
      published_at     = excluded.published_at
  `).run(
    post.source,
    post.source_id,
    post.slug,
    post.title,
    post.content_html,
    post.content_markdown ?? null,
    post.meta_description ?? null,
    post.image_url ?? null,
    JSON.stringify(post.tags),
    post.published_at,
  );
}

function hydrate(row: Record<string, unknown>): BlogPost {
  return { ...row, tags: JSON.parse((row.tags as string) ?? "[]") } as BlogPost;
}

export function listPosts(limit = 50, offset = 0): BlogPost[] {
  const rows = getDb()
    .prepare("SELECT * FROM blog_posts ORDER BY published_at DESC LIMIT ? OFFSET ?")
    .all(limit, offset) as Record<string, unknown>[];
  return rows.map(hydrate);
}

export function getPost(slug: string): BlogPost | null {
  const row = getDb()
    .prepare("SELECT * FROM blog_posts WHERE slug = ? LIMIT 1")
    .get(slug) as Record<string, unknown> | null;
  return row ? hydrate(row) : null;
}
