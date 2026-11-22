import { IDENTITY_PREFIX } from "@/lib/chat/storage";
import { preflight, storageErr, storageJson, storageRate, storageUnauthorized, storageUser } from "@/lib/chat/storage-http";
import { storageIngestUrl, storageStore } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/storage/identity (storage:write): the public key an app seals
 * to. `x25519` is the raw 32-byte X25519 public key, base64url (the part of
 * the identity after "c0ps1."). 409 when nothing is linked yet.
 */
export async function GET(req: Request) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  if (!storageRate(user.id).ok) return storageJson({ error: { code: "rate_limited", message: "slow down" } }, 429);
  try {
    const id = await storageStore().identity(user.id);
    if (!id) {
      return storageJson(
        { linked: false, error: { code: "not_linked", message: "this account has no storage identity linked yet: the person runs `c0mpute storage key link`" } },
        409,
      );
    }
    return storageJson({
      linked: true,
      identity: id.identity,
      x25519: id.identity.slice(IDENTITY_PREFIX.length),
      fingerprint: id.fingerprint,
      linked_at: id.linked_at,
      format: "c0mpute-private-v1",
      ingest_url: storageIngestUrl(),
    });
  } catch (e) {
    return storageErr(e);
  }
}

export const OPTIONS = preflight;
