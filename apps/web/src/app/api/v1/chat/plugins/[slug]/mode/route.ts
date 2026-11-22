import { pluginErr } from "@/lib/chat/plugins";
import { policyFor } from "@/lib/chat/plugin-policy";
import { publicPlugin } from "@/lib/chat/plugin-store";
import { currentUser, json, sameOrigin, siteUrl, pluginStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/chat/plugins/<slug>/mode {mode: "paper" | "live"}: which account
 * a brokerage plugin's orders go to. New connections start on paper; live has
 * to be asked for by name ({mode: "live", confirm: true}), and every order
 * still needs its own approval in the chat.
 */
export async function POST(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const { slug } = await params;
  const body = (await req.json().catch(() => null)) as { mode?: unknown; confirm?: unknown } | null;
  const mode = body?.mode;
  if (mode !== "paper" && mode !== "live") return json({ error: { message: 'mode must be "paper" or "live"', code: 400 } }, 400);
  if (mode === "live" && body?.confirm !== true) {
    return json({ error: { message: 'switching to live trading needs {"mode":"live","confirm":true}: orders then use real money (each one still asks first)', code: 400 } }, 400);
  }
  try {
    const ps = pluginStore();
    const p = await ps.get(user.id, slug);
    if (!p) return json({ error: { message: `no chat plugin ${slug} on this account`, code: 404 } }, 404);
    if (policyFor(p.url)?.kind !== "trading") return json({ error: { message: `${slug} is not a brokerage plugin`, code: 400 } }, 400);
    const out = await ps.setTradingMode(user.id, slug, mode);
    return json({ plugin: publicPlugin(out!, siteUrl()) });
  } catch (e) {
    return pluginErr(e);
  }
}
