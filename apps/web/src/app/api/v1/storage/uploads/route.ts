import { appName, preflight, storageErr, storageJson, storageRate, storageUnauthorized, storageUser } from "@/lib/chat/storage-http";
import { storageIngestUrl, storageStore } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/storage/uploads (storage:write) `{bytes, objects?}`: open an
 * upload session for one file. `bytes` is the total ciphertext the app will
 * send (every part and the root object); `objects` how many sealed objects
 * (parts + root). Returns a ticket for the ingest gateway:
 * PUT `${ingest_url}/storage/v1/sealed/<site_id>?offset=N` with
 * `Authorization: Bearer <ticket>`, then POST `.../finish`.
 */
export async function POST(req: Request) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  if (!storageRate(user.id).ok) return storageJson({ error: { code: "rate_limited", message: "slow down" } }, 429);
  const body = ((await req.json().catch(() => null)) ?? {}) as Record<string, unknown>;
  try {
    const up = await storageStore().startUpload(user.id, body, appName(user));
    return storageJson({ ...up, ingest_url: storageIngestUrl(), part_bytes: 192 * 1024 * 1024 }, 201);
  } catch (e) {
    return storageErr(e);
  }
}

/** GET /api/v1/storage/uploads: this account's finished app uploads (newest first). */
export async function GET(req: Request) {
  const user = await storageUser(req);
  if (!user) return storageUnauthorized();
  try {
    const rows = await storageStore().listUploads(user.id);
    return storageJson({
      uploads: rows.map((u) => ({
        id: u.id,
        ref: u.root_site_id ? `c0p1:${u.root_site_id}` : null,
        app: u.app,
        bytes: u.bytes,
        objects: u.site_ids.length,
        created_at: u.created_at,
        completed_at: u.completed_at,
      })),
    });
  } catch (e) {
    return storageErr(e);
  }
}

export const OPTIONS = preflight;
