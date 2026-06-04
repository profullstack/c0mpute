CREATE TABLE `blog_posts` (
	`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL,
	`source` text DEFAULT 'crawlproof' NOT NULL,
	`source_id` text NOT NULL,
	`slug` text NOT NULL,
	`title` text NOT NULL,
	`content_html` text NOT NULL,
	`content_markdown` text,
	`meta_description` text,
	`image_url` text,
	`tags` text DEFAULT '[]' NOT NULL,
	`published_at` text NOT NULL,
	`created_at` text DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')) NOT NULL
);
