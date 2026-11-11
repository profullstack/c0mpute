import { CORS, OAuthError } from "@/lib/chat/oauth";
import { clientIp, json, oauthStore, rateLimit } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** RFC 7591 dynamic client registration (public clients, PKCE). */
export async function POST(req: Request) {
  if (!rateLimit(`oauth-register:${clientIp(req)}`, 60).ok) return json({ error: "slow_down" }, 429, CORS);
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body || typeof body !== "object") return json({ error: "invalid_client_metadata", error_description: "JSON body required" }, 400, CORS);
  try {
    const c = await oauthStore().registerClient(body);
    return json(
      {
        client_id: c.client_id,
        client_id_issued_at: Math.floor(Date.parse(c.created_at) / 1000),
        client_name: c.name,
        redirect_uris: c.redirect_uris,
        grant_types: ["authorization_code"],
        response_types: ["code"],
        token_endpoint_auth_method: "none",
      },
      201,
      CORS,
    );
  } catch (e) {
    if (e instanceof OAuthError) return json({ error: e.code, error_description: e.message }, e.status, CORS);
    console.error("oauth register", e);
    return json({ error: "server_error" }, 503, CORS);
  }
}

export function OPTIONS() {
  return new Response(null, { status: 204, headers: CORS });
}
