import { badgeKey, badgeSvg, badgeTheme, countHit } from "@/lib/gallery/badge";
import { gallery } from "@/lib/gallery/server";

export const dynamic = "force-dynamic";

const HEADERS = {
  "content-type": "image/svg+xml; charset=utf-8",
  // The image is the same for every site, so caches may keep it a day; hit
  // counts are cache misses, which is all the tracking there is.
  "cache-control": "public, max-age=3600, s-maxage=86400, stale-while-revalidate=604800",
  "access-control-allow-origin": "*",
  "cross-origin-resource-policy": "cross-origin",
  "x-content-type-options": "nosniff",
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'",
  vary: "Accept-Encoding",
};

/**
 * GET /badge.svg?url=<site url>[&theme=dark]: the "Listed on the c0mpute
 * gallery" badge, keyed by the site's URL (no product id). Without ?url= the
 * embedding page's Referer stands in. Counts one hit for that URL and, when
 * that site is waiting in the free queue, looks for the badge right away.
 */
export async function GET(req: Request) {
  const u = new URL(req.url);
  const key = badgeKey(u.searchParams.get("url"), req.headers.get("referer"));
  if (key) {
    countHit(key);
    gallery().nudge(key).catch(() => {});
  }
  return new Response(badgeSvg(badgeTheme(u.searchParams.get("theme"))), { headers: HEADERS });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: { "access-control-allow-origin": "*", "access-control-allow-methods": "GET, OPTIONS" } });
}
