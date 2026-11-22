import { gerror, gjson, goptions } from "@/lib/gallery/http";
import { gallery } from "@/lib/gallery/server";
import { publicStatus } from "@/lib/gallery/service";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/gallery/submissions/<id>/checkout: open a CoinPay checkout for
 * priority review (a fresh session each time: sessions expire in an hour).
 * Works for a free listing still in the queue too (an upgrade).
 */
export async function POST(req: Request, ctx: { params: Promise<{ id: string }> }) {
  const { id } = await ctx.params;
  try {
    const l = await gallery().store.byId(id);
    if (!l) return gjson({ error: { message: "no such listing", code: 404 } }, 404);
    const s = await gallery().openCheckout(l);
    // A plain form post goes straight to CoinPay; fetch/CLI get JSON.
    if ((req.headers.get("content-type") ?? "").includes("form")) return Response.redirect(s.url, 303);
    return gjson({ listing: publicStatus((await gallery().store.byId(id))!), checkout_url: s.url });
  } catch (e) {
    return gerror(e);
  }
}

export const OPTIONS = goptions;
