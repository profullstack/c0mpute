import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { expertsErrorResponse, expertsService } from "@/lib/experts/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/experts/checkout   { plan: "all" | "<expert slug>", email }
 * Opens a CoinPay checkout for one period of the plan and returns { url }.
 * Paid plans are bound to the CoinPay account (sub); the email is only where
 * CoinPay sends the receipt.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const raw = (await req.json().catch(() => null)) as { plan?: unknown; email?: unknown } | null;
  const plan = typeof raw?.plan === "string" ? raw.plan.trim() : "";
  const email = typeof raw?.email === "string" ? raw.email.trim() : "";
  try {
    return json(await expertsService().checkout(user, plan || "all", email), 201);
  } catch (e) {
    return expertsErrorResponse(e);
  }
}
