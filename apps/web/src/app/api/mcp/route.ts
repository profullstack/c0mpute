import { handleRpc } from "@/lib/chat/mcp";
import { CORS } from "@/lib/chat/oauth";
import { addPlugin, catalogFor, connectLink, hasEnabledPlugins, runPluginChat } from "@/lib/chat/plugins";
import { gallery } from "@/lib/gallery/server";
import { accountStore, fleetFor, fleetService } from "@/lib/accounts/server";
import { isFleetAction, runAction, selectNodes } from "@/lib/accounts/fleet";
import { earningsStore } from "@/lib/earnings/server";
import { recordUsage } from "@/lib/leaderboard/server";
import { convoStore, currentUser, infernetFor, json, mcpUnauthorized, ownsJob, pluginStore, pushStore, rateLimit, recordJob, siteUrl } from "@/lib/chat/server";

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
  const user = await currentUser(req, { oauth: true });
  if (!user) return mcpUnauthorized();
  const body = await req.json().catch(() => null);
  if (!body) return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }, 400);
  const batch = Array.isArray(body) ? body : [body];
  // A message must be an object; anything else is an invalid request, not a crash.
  if (batch.length === 0 || batch.some((m) => !m || typeof m !== "object" || Array.isArray(m))) {
    return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "invalid request" } }, 400);
  }
  const calls = batch.filter((m) => m?.method === "tools/call").length;
  for (let i = 0; i < calls; i++) {
    if (!rateLimit(user.id).ok) {
      return json({ jsonrpc: "2.0", id: null, error: { code: -32000, message: "rate limit: too many chats this hour" } }, 429);
    }
  }
  // One count per tool call for the public leaderboard: the tool name, never its arguments.
  for (const m of batch) if (m?.method === "tools/call") recordUsage(user.id, "mcp", (m.params as { name?: unknown } | undefined)?.name);
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
  const out = (await Promise.all(batch.map((m) => handleRpc(m, { user, upstream, push: pushStore(), plugins, signal: req.signal, convos: convoStore(), jobs: { record: (u, k, j) => recordJob(u, k, j), owns: ownsJob }, gallery: gallery(), fleet: fleetDeps(user) })))).filter(Boolean);
  if (out.length === 0) return new Response(null, { status: 202 });
  return json(Array.isArray(body) ? out : out[0]);
}

/** Fleet tools for this caller: the same actions as POST /api/v1/fleet/actions, audit-logged as "mcp". */
function fleetDeps(user: { coinpay_sub: string | null; did: string | null; name: string | null }) {
  return {
    site: siteUrl(),
    fleet: (userId: string) => fleetFor(userId),
    account: async () => ({ coinpay: !!user.coinpay_sub, did: user.did, name: user.name, sign_in: null }),
    action: async (userId: string, body: { action: string; nodes?: unknown; tag?: unknown; all?: unknown; args?: Record<string, unknown> }) => {
      if (!isFleetAction(body.action)) return { error: "action: pause, resume, update, restart, rename, tag, untag, set-tags, cost or unlink" };
      const f = await fleetFor(userId);
      const targets = selectNodes(f.nodes, body);
      if (typeof targets === "string") return { error: targets };
      if (!targets.length) return { error: "no nodes matched" };
      return runAction({ userId, actor: "mcp", action: body.action, args: body.args ?? {}, targets, fleet: fleetService(), accounts: accountStore(), earnings: earningsStore() });
    },
  };
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}

export async function GET() {
  // No server-initiated stream.
  return new Response(null, { status: 405, headers: { allow: "POST" } });
}
