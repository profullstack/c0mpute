import { listPosts } from "@/lib/blog-db";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

const SITE = "https://c0mpute.com";

export async function GET() {
  const posts = listPosts(50);

  const items = posts
    .map((p) => {
      const pubDate = new Date(p.published_at).toUTCString();
      const link = `${SITE}/blog/${p.slug}`;
      const desc = p.meta_description
        ? `<![CDATA[${p.meta_description}]]>`
        : `<![CDATA[${p.title}]]>`;
      return `    <item>
      <title><![CDATA[${p.title}]]></title>
      <link>${link}</link>
      <guid isPermaLink="true">${link}</guid>
      <pubDate>${pubDate}</pubDate>
      <description>${desc}</description>
    </item>`;
    })
    .join("\n");

  const xml = `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
  <channel>
    <title>c0mpute blog</title>
    <link>${SITE}/blog</link>
    <description>Updates from the c0mpute decentralized compute network.</description>
    <language>en</language>
    <atom:link href="${SITE}/blog/rss.xml" rel="self" type="application/rss+xml"/>
${items}
  </channel>
</rss>`;

  return new Response(xml, {
    headers: {
      "Content-Type": "application/rss+xml; charset=utf-8",
      "Cache-Control": "public, max-age=3600",
    },
  });
}
