import { galleryResponse } from "@/lib/gallery";

// Static list, so cache it at the edge for an hour; it changes only on deploy.
export const revalidate = 3600;

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
export function GET() {
  return Response.json(galleryResponse(), {
    headers: { ...CORS, "Cache-Control": "public, max-age=300, s-maxage=3600" },
  });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
