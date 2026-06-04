import { NextRequest, NextResponse } from "next/server";
import { timingSafeEqual } from "node:crypto";
import { revalidatePath } from "next/cache";
import { upsertPost } from "@/lib/blog-db";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

function bearerMatches(token: string, secret: string): boolean {
  try {
    const a = Buffer.from(token);
    const b = Buffer.from(secret);
    if (a.length !== b.length) return false;
    return timingSafeEqual(a, b);
  } catch {
    return false;
  }
}

export async function POST(req: NextRequest) {
  const secret = process.env.CRAWLPROOF_AUTOBLOG_WEBHOOK_SECRET;
  if (!secret) {
    console.error("[autoblog webhook] CRAWLPROOF_AUTOBLOG_WEBHOOK_SECRET not set");
    return NextResponse.json({ error: "Webhook not configured" }, { status: 500 });
  }

  const auth = req.headers.get("authorization") ?? "";
  const bearer = auth.replace(/^Bearer\s+/i, "").trim();
  if (!bearer || !bearerMatches(bearer, secret)) {
    return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
  }

  let body: unknown;
  try {
    body = await req.json();
  } catch {
    return NextResponse.json({ error: "Invalid JSON" }, { status: 400 });
  }

  // CloudEvents 1.0 envelope from @profullstack/autoblog: { data: { post: {...} } }
  // Fall back through data.post → data → raw body for forward-compat.
  const envelope = body as Record<string, unknown>;
  const dataField = envelope?.data as Record<string, unknown> | undefined;
  const post = dataField?.post ?? dataField ?? body;
  const p = post as Record<string, unknown>;

  const source_id  = p.id    as string | undefined;
  const slug       = p.slug  as string | undefined;
  const title      = p.title as string | undefined;
  const html       = p.html  as string | undefined;

  if (!source_id || !slug || !title || !html) {
    return NextResponse.json(
      { error: "Missing required fields: id, slug, title, html" },
      { status: 422 },
    );
  }

  try {
    await upsertPost({
      source:           "crawlproof",
      source_id:        String(source_id),
      slug:             String(slug),
      title:            String(title),
      content_html:     String(html),
      content_markdown: (p.markdown as string | null) ?? null,
      meta_description: (p.excerpt  as string | null) ?? null,
      image_url:        ((p.featured_image as Record<string, string> | null)?.url) ?? null,
      tags:             Array.isArray(p.tags) ? (p.tags as string[]) : [],
      published_at:     (p.published_at as string | null) ?? new Date().toISOString(),
    });
  } catch (err) {
    console.error("[autoblog webhook] upsert failed:", err);
    return NextResponse.json({ error: "Failed to persist post" }, { status: 500 });
  }

  revalidatePath("/blog");
  revalidatePath(`/blog/${slug}`);

  console.log(`[autoblog webhook] upserted: ${slug}`);
  return NextResponse.json({ ok: true, slug });
}
