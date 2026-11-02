import { json, sameOrigin, SESSION_COOKIE, sessionToken, store } from "@/lib/chat/server";

export async function POST(req: Request) {
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  const t = sessionToken(req);
  if (t) await store().revokeToken(t).catch(() => {});
  return json({ ok: true }, 200, {
    "set-cookie": `${SESSION_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0`,
  });
}
