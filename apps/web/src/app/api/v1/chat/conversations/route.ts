import { convoErr } from "@/lib/chat/convo-http";
import { convoStore, currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/chat/conversations[?since=<ms>]: this account's synced
 * conversations, newest first (no messages). With `since`, only what changed
 * after it, deletes included (`deleted: true`), for incremental sync.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const since = Number(new URL(req.url).searchParams.get("since") ?? 0);
  try {
    const data = await convoStore().list(user.id, Number.isFinite(since) && since > 0 ? since : undefined);
    return json({ data, now: Date.now() });
  } catch (e) {
    return convoErr(e);
  }
}
