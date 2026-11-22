import { safeNext } from "@/lib/accounts/util";
import { coinpay, currentUser, pkce } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * Start "Sign in with CoinPay" (OAuth 2.1: authorization code + PKCE S256 +
 * state), or with ?link=1 "Connect CoinPay" to the account already signed in.
 * The verifier, state and intent wait in a short-lived HttpOnly cookie scoped
 * to the callback path.
 */
export async function GET(req: Request) {
  if (!coinpay.clientId()) return new Response("CoinPay sign-in is not configured", { status: 503 });
  const url = new URL(req.url);
  const next = safeNext(url.searchParams.get("next"), "/chat");
  // Linking is only offered to someone signed in; anyone else just signs in.
  const link = url.searchParams.get("link") === "1" && !!(await currentUser(req));
  const { verifier, challenge, state } = pkce();
  const auth = new URL(`${coinpay.base()}/api/oauth/authorize`);
  auth.search = new URLSearchParams({
    response_type: "code",
    client_id: coinpay.clientId(),
    redirect_uri: coinpay.redirectUri(),
    scope: coinpay.scope(),
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
  }).toString();
  const value = encodeURIComponent(JSON.stringify({ state, verifier, next, ...(link ? { link: true } : {}) }));
  return new Response(null, {
    status: 302,
    headers: {
      location: auth.toString(),
      "set-cookie": `c0_oauth=${value}; Path=/api/auth/coinpay; HttpOnly; Secure; SameSite=Lax; Max-Age=600`,
      "cache-control": "no-store",
    },
  });
}
