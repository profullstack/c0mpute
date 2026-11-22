import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { expertsErrorResponse, expertsService } from "@/lib/experts/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/experts/trial: start this account's free Experts trial now
 * (otherwise the first question starts it). One per CoinPay account, ever:
 * calling again returns the same trial with created: false.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  try {
    const r = await expertsService().startTrial(user);
    return json({ ...r, status: await expertsService().status(user) }, r.created ? 201 : 200);
  } catch (e) {
    return expertsErrorResponse(e);
  }
}
