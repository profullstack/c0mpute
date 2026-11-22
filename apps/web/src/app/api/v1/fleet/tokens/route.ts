import { accountStore } from "@/lib/accounts/server";
import { cleanTags, parseTtl } from "@/lib/accounts/store";
import { currentUser, json, sameOrigin, siteUrl, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const refuse = () => json({ error: { message: "cross-site request refused", code: 403 } }, 403);

/** GET: this account's fleet enrollment tokens (never the secrets). */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ tokens: await accountStore().enrollTokens(user.id) });
}

/**
 * POST {name, expires?: "7d", max_uses?, tags?}: a token that lets nodes link
 * themselves to this account at install time, without a browser:
 *   curl -fsSL https://c0mpute.com/install.sh | sh -s -- --enroll c0e_...
 *   c0mpute node link --token c0e_...
 * It can only link nodes. Shown once; stored hashed; revoke any time.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return refuse();
  const b = (await req.json().catch(() => ({}))) as { name?: unknown; expires?: unknown; max_uses?: unknown; tags?: unknown };
  const name = typeof b.name === "string" && b.name.trim() ? b.name.trim() : "fleet";
  const ttl = parseTtl(b.expires ?? "7d");
  if (ttl === "invalid") return json({ error: { message: "expires: like 30m, 12h, 7d (at most 366d), or never", code: 400 } }, 400);
  const tags = b.tags === undefined ? [] : cleanTags(b.tags);
  if (!tags) return json({ error: { message: "tags: short lowercase words", code: 400 } }, 400);
  const maxUses = b.max_uses == null ? null : Number(b.max_uses);
  if (maxUses !== null && (!Number.isInteger(maxUses) || maxUses < 1 || maxUses > 100_000)) return json({ error: { message: "max_uses: 1-100000", code: 400 } }, 400);
  try {
    const { token, info } = await accountStore().createEnrollToken(user.id, { name, ttlMs: ttl, maxUses, tags });
    await accountStore().audit(user.id, { action: "token:create", args: { name, expires: info.expires_at, tags }, actor: user.token_kind === "session" ? "web" : "api", outcome: "ok" });
    return json(
      {
        token,
        ...info,
        note: "Shown once. Nodes that use it link to your account.",
        install: `curl -fsSL ${siteUrl()}/install.sh | sh -s -- --enroll ${token}`,
        link: `c0mpute node link --token ${token}`,
      },
      201,
    );
  } catch (e) {
    return json({ error: { message: e instanceof Error ? e.message : "could not create the token", code: 409 } }, 409);
  }
}

/** DELETE ?id=<id or name>: revoke. Nodes it already linked stay linked. */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return refuse();
  const id = new URL(req.url).searchParams.get("id") ?? "";
  const n = await accountStore().revokeEnrollToken(user.id, id);
  if (n) await accountStore().audit(user.id, { action: "token:revoke", args: { id }, actor: user.token_kind === "session" ? "web" : "api", outcome: `revoked ${n}` });
  return n ? json({ ok: true, revoked: n }) : json({ error: { message: "no live token with that id or name", code: 404 } }, 404);
}
