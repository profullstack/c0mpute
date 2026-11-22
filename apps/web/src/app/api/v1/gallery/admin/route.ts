import { currentUser } from "@/lib/chat/server";
import { gerror, gjson } from "@/lib/gallery/http";
import { gallery, isGalleryAdmin } from "@/lib/gallery/server";
import { adminView } from "@/lib/gallery/service";
import type { ListingStatus } from "@/lib/gallery/store";

export const dynamic = "force-dynamic";

const STATUSES = new Set(["all", "queued", "awaiting_payment", "priority_review", "review", "approved", "rejected"]);

/** GET /api/v1/gallery/admin?status=<status|all>: every listing with contact and payment fields. Moderators only. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!(await isGalleryAdmin(user))) return gjson({ error: { message: "moderators only", code: 403 } }, user ? 403 : 401);
  const status = new URL(req.url).searchParams.get("status") ?? "all";
  if (!STATUSES.has(status)) return gjson({ error: { message: `status: ${[...STATUSES].join(", ")}`, code: 400 } }, 400);
  try {
    const rows = await gallery().store.list({ status: status as ListingStatus | "all", limit: 500 });
    return gjson({ object: "list", count: rows.length, data: rows.map(adminView) });
  } catch (e) {
    return gerror(e);
  }
}
