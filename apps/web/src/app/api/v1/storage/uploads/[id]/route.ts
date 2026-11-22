import { replicationStatus } from "@/lib/chat/storage";
import { preflight, storageErr, storageJson, storageRate, storageUnauthorized, storageUser } from "@/lib/chat/storage-http";
import { storageIngestUrl, storageStore } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/storage/uploads/<id>: the session, and live copy counts once it is complete. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  if (!storageRate(user.id).ok) return storageJson({ error: { code: "rate_limited", message: "slow down" } }, 429);
  const { id } = await params;
  try {
    const up = await storageStore().upload(user.id, id);
    if (!up) return storageJson({ error: { code: "not_found", message: "no such upload on this account" } }, 404);
    const live = up.site_ids.length ? await replicationStatus(storageIngestUrl(), up.site_ids) : { status: "uploading", copies: 0, target: 3, objects: [] };
    return storageJson({ upload_id: up.id, ref: up.root_site_id ? `c0p1:${up.root_site_id}` : null, app: up.app, bytes: up.bytes, ...live });
  } catch (e) {
    return storageErr(e);
  }
}

/**
 * POST /api/v1/storage/uploads/<id> `{ref, site_ids}`: the app reports what
 * it published (the root `c0p1:` ref and every part's site id), so the status
 * routes can follow replication and the owner sees it on their account page.
 */
export async function POST(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  if (!storageRate(user.id).ok) return storageJson({ error: { code: "rate_limited", message: "slow down" } }, 429);
  const { id } = await params;
  const body = ((await req.json().catch(() => null)) ?? {}) as Record<string, unknown>;
  try {
    const up = await storageStore().completeUpload(user.id, id, body);
    const live = await replicationStatus(storageIngestUrl(), up.site_ids);
    return storageJson({ upload_id: up.id, ref: `c0p1:${up.root_site_id}`, ...live });
  } catch (e) {
    return storageErr(e);
  }
}

export const OPTIONS = preflight;
