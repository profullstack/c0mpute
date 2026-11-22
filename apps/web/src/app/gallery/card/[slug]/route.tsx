import { ImageResponse } from "next/og";
import { gallery } from "@/lib/gallery/server";
import { displayUrl } from "@/lib/gallery/url";

export const dynamic = "force-dynamic";

/**
 * GET /gallery/card/<slug>: the 1280x800 PNG card for a submitted listing that
 * has no screenshot (the feed's `image` points here). Approved listings only.
 */
export async function GET(_req: Request, ctx: { params: Promise<{ slug: string }> }) {
  const { slug } = await ctx.params;
  const l = await gallery().store.bySlug(slug.replace(/\.png$/, "")).catch(() => null);
  if (!l || l.status !== "approved") return new Response("not found", { status: 404 });
  const tagline = l.tagline.length > 150 ? `${l.tagline.slice(0, 147)}...` : l.tagline;
  const img = new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          justifyContent: "space-between",
          padding: "72px 80px",
          background: "#0a0a0b",
          color: "#d4d4d4",
          fontFamily: "monospace",
          borderTop: "12px solid #4ade80",
        }}
      >
        <div style={{ display: "flex", fontSize: 34, color: "#6b7280" }}>{`// built on c0mpute · ${l.category}`}</div>
        <div style={{ display: "flex", flexDirection: "column", gap: 28 }}>
          <div style={{ display: "flex", fontSize: l.name.length > 28 ? 72 : 92, fontWeight: 700, lineHeight: 1.05, color: "#f5f5f5" }}>{l.name}</div>
          {tagline ? <div style={{ display: "flex", fontSize: 36, lineHeight: 1.35, color: "#a3a3a3" }}>{tagline}</div> : null}
        </div>
        <div style={{ display: "flex", justifyContent: "space-between", fontSize: 32, color: "#6b7280" }}>
          <span>{displayUrl(l.url)}</span>
          <span style={{ color: "#4ade80" }}>c0mpute.com/gallery</span>
        </div>
      </div>
    ),
    { width: 1280, height: 800 },
  );
  img.headers.set("cache-control", "public, max-age=3600, s-maxage=86400");
  return img;
}
