import { gerror, gjson, goptions } from "@/lib/gallery/http";
import { gallery } from "@/lib/gallery/server";
import { publicStatus } from "@/lib/gallery/service";

export const dynamic = "force-dynamic";

/** GET /api/v1/gallery/submissions/<id>: a listing's public status (also accepts its slug). */
export async function GET(_req: Request, ctx: { params: Promise<{ id: string }> }) {
  const { id } = await ctx.params;
  try {
    const s = gallery().store;
    const l = (await s.byId(id)) ?? (await s.bySlug(id));
    if (!l) return gjson({ error: { message: "no such listing", code: 404 } }, 404);
    return gjson({ listing: publicStatus(l, await s.hits(l.url_key)) });
  } catch (e) {
    return gerror(e);
  }
}

export const OPTIONS = goptions;
