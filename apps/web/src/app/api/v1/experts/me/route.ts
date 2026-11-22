import { currentUser, json, unauthorized } from "@/lib/chat/server";
import { expertsErrorResponse, expertsService } from "@/lib/experts/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/experts/me: this account's Experts trial, subscriptions, per-expert access and usage. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    return json(await expertsService().status(user));
  } catch (e) {
    return expertsErrorResponse(e);
  }
}
