import { refToSiteId, replicationStatus } from "@/lib/chat/storage";
import { preflight, storageErr, storageJson, storageRate, storageUnauthorized, storageUser } from "@/lib/chat/storage-http";
import { storageIngestUrl, storageStore } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/storage/objects/<c0p1:id or id>: replication of an object an
 * app uploaded for this account (every part counts: `copies` is the lowest).
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  if (!storageRate(user.id).ok) return storageJson({ error: { code: "rate_limited", message: "slow down" } }, 429);
  const { id } = await params;
  const siteId = refToSiteId(id);
  if (!siteId) return storageJson({ error: { code: "invalid_request", message: "expected c0p1:<20 hex>" } }, 400);
  try {
    const up = await storageStore().uploadByRef(user.id, siteId);
    const ids = up?.site_ids.length ? up.site_ids : [siteId];
    const live = await replicationStatus(storageIngestUrl(), ids);
    return storageJson({ ref: `c0p1:${siteId}`, upload_id: up?.id ?? null, app: up?.app ?? null, ...live });
  } catch (e) {
    return storageErr(e);
  }
}

export const OPTIONS = preflight;
