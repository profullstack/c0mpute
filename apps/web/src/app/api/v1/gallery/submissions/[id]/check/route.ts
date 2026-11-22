import { gerror, gjson, goptions } from "@/lib/gallery/http";
import { gallery } from "@/lib/gallery/server";
import { publicStatus } from "@/lib/gallery/service";

export const dynamic = "force-dynamic";
export const maxDuration = 60;

/** POST /api/v1/gallery/submissions/<id>/check: "check my badge now" (once a minute per listing). */
export async function POST(_req: Request, ctx: { params: Promise<{ id: string }> }) {
  const { id } = await ctx.params;
  try {
    const l = await gallery().store.byId(id);
    if (!l) return gjson({ error: { message: "no such listing", code: 404 } }, 404);
    const r = await gallery().checkNow(l, { manual: true });
    return gjson({ listing: publicStatus(r.listing), badge: r.badge, message: r.message });
  } catch (e) {
    return gerror(e);
  }
}

export const OPTIONS = goptions;
