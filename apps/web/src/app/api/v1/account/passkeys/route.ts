import { accountStore, keepsAWayIn, signInMethods } from "@/lib/accounts/server";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const refuse = () => json({ error: { message: "cross-site request refused", code: 403 } }, 403);

/** GET: this account's passkeys (add one with the passkey register flow while signed in). */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ passkeys: (await signInMethods(user)).passkeys });
}

/** PATCH {id, name}: rename a passkey. */
export async function PATCH(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return refuse();
  const b = (await req.json().catch(() => ({}))) as { id?: unknown; name?: unknown };
  if (typeof b.id !== "string" || typeof b.name !== "string") return json({ error: { message: "id and name are required", code: 400 } }, 400);
  await accountStore().renamePasskey(user.id, b.id, b.name.trim() || "passkey");
  return json({ ok: true });
}

/** DELETE ?id=<credential id>: remove a passkey, unless it is the last way in. */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return refuse();
  const id = new URL(req.url).searchParams.get("id") ?? "";
  if (!(await keepsAWayIn(user, { passkey: id }))) {
    return json({ error: { message: "this is your only way to sign in: add another passkey or connect CoinPay first", code: 409 } }, 409);
  }
  const gone = await accountStore().removePasskey(user.id, id);
  return gone ? json({ ok: true }) : json({ error: { message: "no such passkey on this account", code: 404 } }, 404);
}
