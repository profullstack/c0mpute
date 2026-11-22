import { CORS } from "@/lib/chat/oauth";
import { clientIp, json, rateLimit, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * RFC 7009 token revocation for app-held keys: POST token=<c0k_...> (form or
 * JSON). An app calls it when the person disconnects, so the key stops working
 * at once instead of lingering until they find it in their key list.
 *
 * Holding the token is the authorization (public clients, no secret), and it
 * deletes only an OAuth-issued key, never a session or a full API key. Per the
 * RFC the answer is 200 whether or not the token existed.
 */
export async function POST(req: Request) {
  if (!rateLimit(`oauth-revoke:${clientIp(req)}`, 120).ok) return json({ error: "slow_down" }, 429, CORS);
  const type = req.headers.get("content-type") ?? "";
  let p: Record<string, unknown> = {};
  if (type.includes("application/json")) p = ((await req.json().catch(() => null)) as Record<string, unknown>) ?? {};
  else p = Object.fromEntries(new URLSearchParams(await req.text()));
  if (typeof p.token === "string" && p.token) {
    try {
      await store().revokeOAuthToken(p.token);
    } catch (e) {
      console.error("oauth revoke", e instanceof Error ? e.message : e);
      return json({ error: "server_error" }, 503, CORS);
    }
  }
  return json({}, 200, { ...CORS, "cache-control": "no-store" });
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
