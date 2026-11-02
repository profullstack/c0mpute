import { currentUser, json, sameOrigin, store, unauthorized } from "@/lib/chat/server";

/** `c0mpute chat login`, step 2: the signed-in person approves the code their terminal shows. */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  const body = (await req.json().catch(() => ({}))) as { code?: string };
  const ok = await store().approveDevice(String(body.code ?? ""), user.id);
  return ok ? json({ ok: true }) : json({ error: "unknown or expired code" }, 404);
}
