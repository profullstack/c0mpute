import { storageErr } from "@/lib/chat/storage-http";
import { currentUser, json, rateLimit, sameOrigin, storageStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * Start linking a storage identity: a fresh server X25519 key and nonce
 * (5 minutes, single use). The CLI answers with
 * HKDF(X25519(storage key, server_key), server_key || identity,
 * "c0mpute-storage-link-v1\n" + challenge_id + "\n" + nonce + "\n" + identity).
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  if (!rateLimit(`storage-link:${user.id}`, 30).ok) return json({ error: "slow down" }, 429);
  try {
    return json(await storageStore().startLink(user.id));
  } catch (e) {
    return storageErr(e);
  }
}
