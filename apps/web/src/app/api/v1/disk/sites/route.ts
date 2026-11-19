import { bad } from "@/lib/chat/push-http";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { diskStore } from "@/lib/disk/server";
import { parseSiteClaim } from "@/lib/disk/store";

export const dynamic = "force-dynamic";

/** GET /api/v1/disk/sites: the site ids and owner keys this account claims. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ claims: await diskStore().claims(user.id) });
}

/**
 * POST /api/v1/disk/sites {site: "<site id, site URL, or 64-hex owner key>", label?}
 *
 * Adds a site (or every site of an owner key: `c0mpute hosting ls` shows it)
 * to your storage dashboard. Sites you push from c0mpute chat are there
 * already. Like node claims, this only shows public numbers; no money moves.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const c = parseSiteClaim(body.site);
  if (!c) return bad("give a 20-character site id, a /s/<id>/ link, or a 64-character owner key", 400);
  const label = typeof body.label === "string" ? body.label.trim().slice(0, 64) || null : undefined;
  try {
    await diskStore().claim(user.id, { ...c, label });
  } catch (e) {
    return bad(e instanceof Error ? e.message : String(e), 400);
  }
  return json({ claim: c }, 201);
}

/** DELETE /api/v1/disk/sites?site=<id or owner key> */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const c = parseSiteClaim(new URL(req.url).searchParams.get("site"));
  if (!c) return bad("not a site id or owner key", 400);
  const gone = await diskStore().unclaim(user.id, c.kind, c.value);
  return gone ? json({ ok: true }) : bad("not a site you added (chat pushes are removed with /push rm)", 404);
}
