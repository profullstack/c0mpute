import { CORS, OAuthError } from "@/lib/chat/oauth";
import { clientIp, json, oauthStore, rateLimit, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * Token endpoint: authorization_code + PKCE in, a c0k_ API key out. The key
 * does not expire; the person revokes it with their other keys in c0mpute chat.
 */
export async function POST(req: Request) {
  if (!rateLimit(`oauth-token:${clientIp(req)}`, 120).ok) return json({ error: "slow_down" }, 429, CORS);
  const type = req.headers.get("content-type") ?? "";
  let p: Record<string, unknown> = {};
  if (type.includes("application/json")) p = ((await req.json().catch(() => null)) as Record<string, unknown>) ?? {};
  else p = Object.fromEntries(new URLSearchParams(await req.text()));
  if (p.grant_type !== "authorization_code") {
    return json({ error: "unsupported_grant_type", error_description: "only authorization_code" }, 400, CORS);
  }
  try {
    const oauth = oauthStore();
    const r = await oauth.redeemCode(p.code, { client_id: p.client_id, redirect_uri: p.redirect_uri, code_verifier: p.code_verifier });
    const client = await oauth.client(r.client_id);
    const key = await store().createKey(r.user_id, `${client?.name ?? "MCP client"} (OAuth)`);
    return json({ access_token: key, token_type: "Bearer", scope: r.scope }, 200, { ...CORS, pragma: "no-cache" });
  } catch (e) {
    if (e instanceof OAuthError) return json({ error: e.code, error_description: e.message }, e.status, CORS);
    console.error("oauth token", e);
    return json({ error: "server_error" }, 503, CORS);
  }
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
