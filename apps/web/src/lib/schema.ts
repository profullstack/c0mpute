import { sqliteTable, integer, text } from "drizzle-orm/sqlite-core";
import { sql } from "drizzle-orm";

export const blogPosts = sqliteTable("blog_posts", {
  id: integer("id").primaryKey({ autoIncrement: true }),
  source: text("source").notNull().default("crawlproof"),
  source_id: text("source_id").notNull(),
  slug: text("slug").notNull(),
  title: text("title").notNull(),
  content_html: text("content_html").notNull(),
  content_markdown: text("content_markdown"),
  meta_description: text("meta_description"),
  image_url: text("image_url"),
  tags: text("tags").notNull().default("[]"),
  published_at: text("published_at").notNull(),
  created_at: text("created_at")
    .notNull()
    .default(sql`(strftime('%Y-%m-%dT%H:%M:%SZ','now'))`),
});
