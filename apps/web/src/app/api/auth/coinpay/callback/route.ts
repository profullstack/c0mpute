import { handleCallback, readSaved } from "@/lib/accounts/coinpay-callback";
import { signInCookie } from "@/lib/accounts/server";
import { coinpayUserinfo, currentUser, exchangeCode, linkInfernetAccount, siteUrl, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const CLEAR = "c0_oauth=; Path=/api/auth/coinpay; HttpOnly; Secure; SameSite=Lax; Max-Age=0";

export async function GET(req: Request) {
  const saved = readSaved(req.headers.get("cookie"));
  const r = await handleCallback(new URL(req.url), saved, {
    users: store(),
    exchange: exchangeCode,
    userinfo: coinpayUserinfo,
    current: saved.link ? await currentUser(req) : null,
    // Make sure the network that serves this person can bill them: without an
    // Infernet account bound to their CoinPay sub, their balance and their
    // nodes' earnings cannot pay for anything here. Non-fatal by design.
    afterSignIn: (info) => linkInfernetAccount(info),
  });
  const headers = new Headers({ location: `${siteUrl()}${r.redirect}`, "cache-control": "no-store" });
  headers.append("set-cookie", CLEAR);
  if (r.user) headers.append("set-cookie", await signInCookie(r.user));
  return new Response(null, { status: 302, headers });
}
