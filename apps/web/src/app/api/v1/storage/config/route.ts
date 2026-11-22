import { STORAGE_SCOPE, ticketKey } from "@/lib/chat/storage";
import { preflight, storageJson } from "@/lib/chat/storage-http";
import { env, siteUrl, storageIngestUrl, storageStore } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/storage/config (public): what an app needs to publish into a
 * user's private storage, and the ticket issuer key a gateway operator sets
 * as `[hosting] sealed_ingest_issuer` to take app uploads.
 */
export function GET() {
  const k = ticketKey(env("C0MPUTE_STORAGE_TICKET_KEY"));
  return storageJson({
    enabled: !!k,
    format: "c0mpute-private-v1",
    scope: STORAGE_SCOPE,
    authorization_server: `${siteUrl()}/.well-known/oauth-authorization-server`,
    ingest_url: storageIngestUrl(),
    ticket_issuer: k?.publicHex ?? null,
    limits: { ...storageStore().limits, max_object_bytes: 200 * 1024 * 1024, part_bytes: 192 * 1024 * 1024, piece_bytes: 16 * 1024 * 1024 },
    npm: "@profullstack/c0mpute-sealed",
    docs: "https://github.com/profullstack/c0mpute/blob/master/docs/private-storage.md#publish-from-apps",
  });
}

export const OPTIONS = preflight;
