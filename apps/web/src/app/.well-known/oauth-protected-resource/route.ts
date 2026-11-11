import { CORS, protectedResourceMetadata } from "@/lib/chat/oauth";
import { json, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** RFC 9728: the MCP server at /api/mcp is protected by c0mpute.com's own OAuth server. */
export function GET() {
  return json(protectedResourceMetadata(siteUrl()), 200, { ...CORS, "cache-control": "public, max-age=3600" });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
