import { PasskeyError } from "@/lib/accounts/passkeys";
import { passkeys, signInCookie } from "@/lib/accounts/server";
import { clientIp, currentUser, json, ownOrigin, rateLimit } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * Passkeys (WebAuthn), four steps:
 *   POST /api/auth/passkey/register-options {name?}   -> {options, ceremony}
 *   POST /api/auth/passkey/register-verify  {ceremony, response, name?}
 *        signed in: adds the passkey to this account; signed out: creates an account and signs in
 *   POST /api/auth/passkey/login-options               -> {options, ceremony}
 *   POST /api/auth/passkey/login-verify     {ceremony, response} -> signs in
 * Browser only (our own pages): WebAuthn is bound to the c0mpute.com origin anyway.
 */
export async function POST(req: Request, ctx: { params: Promise<{ step: string }> }) {
  const { step } = await ctx.params;
  if (!ownOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  if (!rateLimit(`passkey:${clientIp(req)}`, 120).ok) return json({ error: { message: "too many attempts; wait a bit", code: 429 } }, 429);
  const body = (await req.json().catch(() => ({}))) as { name?: unknown; ceremony?: unknown; response?: unknown };
  const name = typeof body.name === "string" ? body.name.slice(0, 80) : null;
  try {
    const user = await currentUser(req);
    switch (step) {
      case "register-options": {
        // New accounts by passkey are rate limited per address.
        if (!user && !rateLimit(`signup:${clientIp(req)}`, 20).ok) return json({ error: { message: "too many new accounts from here; wait a bit", code: 429 } }, 429);
        return json(await passkeys().registerOptions(user, { name }));
      }
      case "register-verify": {
        const r = await passkeys().registerVerify(String(body.ceremony ?? ""), body.response, { current: user, name });
        // A new account is signed in at once; adding a passkey keeps the current session.
        const headers: Record<string, string> = r.created ? { "set-cookie": await signInCookie(r.user) } : {};
        return json({ ok: true, created: r.created, account: { id: r.user.id, name: r.user.name } }, 200, headers);
      }
      case "login-options":
        return json(await passkeys().loginOptions());
      case "login-verify": {
        const u = await passkeys().loginVerify(String(body.ceremony ?? ""), (body.response ?? {}) as Record<string, unknown>);
        return json({ ok: true, account: { id: u.id, name: u.name, coinpay: !!u.coinpay_sub } }, 200, { "set-cookie": await signInCookie(u) });
      }
      default:
        return json({ error: { message: "unknown step", code: 404 } }, 404);
    }
  } catch (e) {
    if (e instanceof PasskeyError) return json({ error: { message: e.message, code: e.status } }, e.status);
    console.error("passkey", step, e);
    return json({ error: { message: "passkey service unavailable; try again", code: 503 } }, 503);
  }
}
