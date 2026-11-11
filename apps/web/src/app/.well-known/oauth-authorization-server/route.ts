import { authServerMetadata, CORS } from "@/lib/chat/oauth";
import { json, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** RFC 8414: where MCP clients (ChatGPT, claude.ai, Claude Code) register, sign in and get a token. */
export function GET() {
  return json(authServerMetadata(siteUrl()), 200, { ...CORS, "cache-control": "public, max-age=3600" });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
