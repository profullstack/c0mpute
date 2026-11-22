import { galleryResponse } from "@/lib/gallery";
import { galleryEntries } from "@/lib/gallery/server";

// Listings now change without a deploy (submissions, badge checks), so this is
// rendered per request and cached briefly at the edge instead of for an hour.
export const dynamic = "force-dynamic";

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
  "Access-Control-Max-Age": "86400",
};

/**
 * Public, CORS-open list of things built with or hosted on c0mpute.
 * Shape: docs/gallery-api.md. Read by c0mpute.com/gallery and infernetprotocol.com/gallery.
 */
export async function GET() {
  const { entries } = await galleryEntries();
  return Response.json(galleryResponse(entries), {
    headers: { ...CORS, "Cache-Control": "public, max-age=60, s-maxage=300, stale-while-revalidate=3600" },
  });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
