import { storageErr } from "@/lib/chat/storage-http";
import { currentUser, json, sameOrigin, storageStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * The storage identity linked to this account (`c0mpute storage key link`).
 * Sessions and full API keys only: an OAuth app never links or unlinks.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    const id = await storageStore().identity(user.id);
    return json(id ? { linked: true, ...id } : { linked: false });
  } catch (e) {
    return storageErr(e);
  }
}

/**
 * Finish linking: `{challenge_id, identity, proof}` from
 * POST /api/v1/storage/link/challenge. The proof shows the caller holds the
 * X25519 storage key behind `identity`; only the public identity is stored.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  const body = ((await req.json().catch(() => null)) ?? {}) as Record<string, unknown>;
  try {
    const linked = await storageStore().finishLink(user.id, body);
    return json({ linked: true, ...linked, account: user.name || user.did });
  } catch (e) {
    return storageErr(e);
  }
}

/** Unlink: apps can no longer upload for this account. Uploaded objects stay the owner's. */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  try {
    await storageStore().unlink(user.id);
    return json({ linked: false });
  } catch (e) {
    return storageErr(e);
  }
}
