import { coinpay, pkce } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** Start "Sign in with CoinPay": PKCE + state in a short-lived cookie, then CoinPay's consent page. */
export async function GET(req: Request) {
  if (!coinpay.clientId()) return new Response("CoinPay sign-in is not configured", { status: 503 });
  const url = new URL(req.url);
  const raw = url.searchParams.get("next") || "/chat";
  // Only same-site paths: "//evil.com" and "/\evil.com" are off-site.
  const next = /^\/(?![\/\\])/.test(raw) ? raw : "/chat";
  const { verifier, challenge, state } = pkce();
  const auth = new URL(`${coinpay.base()}/api/oauth/authorize`);
  auth.search = new URLSearchParams({
    response_type: "code",
    client_id: coinpay.clientId(),
    redirect_uri: coinpay.redirectUri(),
    scope: "openid profile did",
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
  }).toString();
  const value = encodeURIComponent(JSON.stringify({ state, verifier, next }));
  return new Response(null, {
    status: 302,
    headers: {
      location: auth.toString(),
      "set-cookie": `c0_oauth=${value}; Path=/api/auth/coinpay; HttpOnly; Secure; SameSite=Lax; Max-Age=600`,
      "cache-control": "no-store",
    },
  });
}
