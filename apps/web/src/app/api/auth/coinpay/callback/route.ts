import { NoDidError } from "@/lib/chat/store";
import { coinpayUserinfo, exchangeCode, SESSION_COOKIE, siteUrl, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const CLEAR = "c0_oauth=; Path=/api/auth/coinpay; HttpOnly; Secure; SameSite=Lax; Max-Age=0";

function back(path: string, extra: string[] = []) {
  const headers = new Headers({ location: `${siteUrl()}${path}`, "cache-control": "no-store" });
  headers.append("set-cookie", CLEAR);
  for (const c of extra) headers.append("set-cookie", c);
  return new Response(null, { status: 302, headers });
}

export async function GET(req: Request) {
  const url = new URL(req.url);
  const raw = (req.headers.get("cookie") ?? "").split(";").map((s) => s.trim()).find((s) => s.startsWith("c0_oauth="));
  let saved: { state?: string; verifier?: string; next?: string } = {};
  try {
    saved = JSON.parse(decodeURIComponent(raw?.slice("c0_oauth=".length) ?? ""));
  } catch {}
  const code = url.searchParams.get("code");
  const err = url.searchParams.get("error");
  if (err) return back(`/chat?error=${encodeURIComponent(err)}`);
  if (!code || !saved.state || saved.state !== url.searchParams.get("state") || !saved.verifier) {
    return back("/chat?error=state");
  }
  try {
    const token = await exchangeCode(code, saved.verifier);
    const info = await coinpayUserinfo(token);
    const user = await store().upsertCoinpayUser(info);
    const session = await store().createSession(user.id);
    const next = saved.next && /^\/(?![\/\\])/.test(saved.next) ? saved.next : "/chat";
    return back(next, [
      `${SESSION_COOKIE}=${session}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=${30 * 86400}`,
    ]);
  } catch (e) {
    if (e instanceof NoDidError) return back("/chat?error=no_did");
    console.error("coinpay callback failed", e);
    return back("/chat?error=signin");
  }
}
