import { breachCount, decoyHash, hashPassword, PASSWORD_DEPRECATION, passwordProblem, validUsername, verifyPassword } from "@/lib/accounts/password";
import { signInCookie } from "@/lib/accounts/server";
import { clientIp, json, ownOrigin, rateLimit, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (message: string, code = 400) => json({ error: { message, code }, deprecated: true }, code);

/**
 * DEPRECATED password sign-in, kept for devices that cannot hold a passkey or
 * open CoinPay. It will be removed; nothing new is built on it.
 *   POST /api/auth/password/login  {username, password}
 *   POST /api/auth/password/signup {username, password, name?}
 * Both answer with `next_step: "add_passkey_or_coinpay"`: the page then asks
 * the person to add a passkey or connect CoinPay.
 */
export async function POST(req: Request, ctx: { params: Promise<{ step: string }> }) {
  const { step } = await ctx.params;
  if (!ownOrigin(req)) return err("cross-site request refused", 403);
  const ip = clientIp(req);
  const body = (await req.json().catch(() => ({}))) as { username?: unknown; password?: unknown; name?: unknown };
  const username = typeof body.username === "string" ? body.username.trim().toLowerCase() : "";
  const password = typeof body.password === "string" ? body.password : "";

  if (step === "login") {
    // Per address and per username: guessing one account from many addresses is capped too.
    if (!rateLimit(`pw-ip:${ip}`, 30).ok || !rateLimit(`pw-user:${username}`, 10).ok) {
      return err("too many sign-in attempts; wait an hour or use a passkey", 429);
    }
    const row = validUsername(username) ? await store().passwordRow(username) : null;
    // Same work whether or not the username exists.
    const ok = await verifyPassword(password, row?.password_hash ?? (await decoyHash()));
    if (!row || !ok) return err("wrong username or password", 401);
    await store().touchLogin(row.id);
    const user = await store().userById(row.id);
    if (!user) return err("wrong username or password", 401);
    return json(
      { ok: true, deprecated: true, notice: PASSWORD_DEPRECATION, next_step: "add_passkey_or_coinpay" },
      200,
      { "set-cookie": await signInCookie(user) },
    );
  }

  if (step === "signup") {
    if (!rateLimit(`signup:${ip}`, 20).ok) return err("too many new accounts from here; wait a bit", 429);
    if (!validUsername(username)) return err("username: 3-32 of a-z 0-9 . _ - starting with a letter or digit");
    const problem = passwordProblem(password);
    if (problem) return err(`password: ${problem}`);
    const breached = await breachCount(password);
    if (breached && breached > 0) return err(`that password appears in ${breached.toLocaleString("en-US")} known data breaches; choose another (or use a passkey)`);
    if (await store().passwordRow(username)) return err("that username is taken", 409);
    const user = await store().createLocalUser(typeof body.name === "string" && body.name.trim() ? body.name.trim() : username);
    if (!(await store().setPassword(user.id, username, await hashPassword(password)))) return err("that username is taken", 409);
    return json(
      { ok: true, deprecated: true, notice: PASSWORD_DEPRECATION, next_step: "add_passkey_or_coinpay" },
      201,
      { "set-cookie": await signInCookie(user) },
    );
  }

  return err("unknown step", 404);
}
