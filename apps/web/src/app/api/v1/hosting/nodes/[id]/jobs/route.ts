import { nodeJob } from "@/lib/chat/push";
import { pushErr } from "@/lib/chat/push-http";
import { currentUser, json, pushStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/hosting/nodes/<id>/jobs: the node's poll. Marks it online and
 * claims its queued pushes and deletes (at most 5 per poll).
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  try {
    const jobs = await pushStore().claimJobs(user.id, id);
    return json({ jobs: jobs.map(nodeJob) });
  } catch (e) {
    return pushErr(e);
  }
}
