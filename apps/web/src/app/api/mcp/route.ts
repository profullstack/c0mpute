import { handleRpc } from "@/lib/chat/mcp";
import { CORS } from "@/lib/chat/oauth";
import { addPlugin, catalogFor, connectLink, hasEnabledPlugins, runPluginChat } from "@/lib/chat/plugins";
import { currentUser, infernetFor, json, mcpUnauthorized, pluginStore, pushStore, rateLimit, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

/**
 * Remote MCP server (Streamable HTTP, JSON responses).
 *   claude mcp add --transport http c0mpute https://c0mpute.com/api/mcp   (OAuth sign-in)
 *   or --header "Authorization: Bearer c0k_..." with a key from c0mpute.com/chat.
 * ChatGPT apps and claude.ai connectors sign in through OAuth 2.1 (lib/chat/oauth.ts),
 * which hands them a c0k_ key. Bearer keys only: no cookies, so a web page
 * cannot drive it cross-site.
 */
export async function POST(req: Request) {
  if (!/^Bearer\s/i.test(req.headers.get("authorization") ?? "")) return mcpUnauthorized();
  const user = await currentUser(req);
  if (!user) return mcpUnauthorized();
  const body = await req.json().catch(() => null);
  if (!body) return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }, 400);
  const batch = Array.isArray(body) ? body : [body];
  const calls = batch.filter((m) => m?.method === "tools/call").length;
  for (let i = 0; i < calls; i++) {
    if (!rateLimit(user.id).ok) {
      return json({ jsonrpc: "2.0", id: null, error: { code: -32000, message: "rate limit: too many chats this hour" } }, 429);
    }
  }
  // Billed to this person's compute balance, same as the HTTP chat API.
  const upstream = infernetFor(user.coinpay_sub);
  const plugins = {
    store: pluginStore(),
    site: siteUrl(),
    catalog: () => catalogFor(siteUrl()),
    add: (userId: string, target: string) => addPlugin(userId, target),
    connectLink,
    hasEnabled: (userId: string) => hasEnabledPlugins(userId),
    run: runPluginChat,
  };
  const out = (await Promise.all(batch.map((m) => handleRpc(m, { user, upstream, push: pushStore(), plugins })))).filter(Boolean);
  if (out.length === 0) return new Response(null, { status: 202 });
  return json(Array.isArray(body) ? out : out[0]);
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}

export async function GET() {
  // No server-initiated stream.
  return new Response(null, { status: 405, headers: { allow: "POST" } });
}
