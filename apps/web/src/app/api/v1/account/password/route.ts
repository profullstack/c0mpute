import { breachCount, hashPassword, PASSWORD_DEPRECATION, passwordProblem, validUsername } from "@/lib/accounts/password";
import { currentUser, json, sameOrigin, store, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * POST {username, password}: set or change this account's password
 * (DEPRECATED method; prefer passkeys or CoinPay). Remove it with
 * DELETE /api/v1/account?method=password.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const b = (await req.json().catch(() => ({}))) as { username?: unknown; password?: unknown };
  const username = typeof b.username === "string" ? b.username.trim().toLowerCase() : "";
  if (!validUsername(username)) return json({ error: { message: "username: 3-32 of a-z 0-9 . _ -", code: 400 } }, 400);
  const problem = passwordProblem(b.password);
  if (problem) return json({ error: { message: `password: ${problem}`, code: 400 } }, 400);
  const breached = await breachCount(b.password as string);
  if (breached && breached > 0) return json({ error: { message: "that password appears in known data breaches; choose another", code: 400 } }, 400);
  if (!(await store().setPassword(user.id, username, await hashPassword(b.password as string)))) {
    return json({ error: { message: "that username is taken", code: 409 } }, 409);
  }
  return json({ ok: true, deprecated: true, notice: PASSWORD_DEPRECATION });
}
