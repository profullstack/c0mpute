import { nodeJob } from "@/lib/chat/push";
import { bad, pushErr } from "@/lib/chat/push-http";
import { currentUser, json, pushStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/hosting/nodes/<id>/jobs: the node's poll. Marks it online and
 * claims its queued pushes and deletes (at most 5 per poll).
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  // A poll claims work, so it is not something a browser session does: with
  // the cookie alone, a link someone followed could claim a node's queue.
  if (user.token_kind !== "key") return bad("only a linked node polls for jobs (use its API key)", 403);
  const { id } = await params;
  try {
    const jobs = await pushStore().claimJobs(user.id, id);
    return json({ jobs: jobs.map(nodeJob) });
  } catch (e) {
    return pushErr(e);
  }
}
