import { pluginErr } from "@/lib/chat/plugins";
import { publicPlugin } from "@/lib/chat/plugin-store";
import { currentUser, json, pluginStore, sameOrigin, siteUrl, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** POST /api/v1/chat/plugins/<slug>/toggle [{enabled: bool}]: on/off in chat (flips when no body). */
export async function POST(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const { slug } = await params;
  const body = (await req.json().catch(() => null)) as { enabled?: unknown } | null;
  try {
    const ps = pluginStore();
    const p = await ps.get(user.id, slug);
    if (!p) return json({ error: { message: `no chat plugin ${slug} on this account`, code: 404 } }, 404);
    const next = typeof body?.enabled === "boolean" ? body.enabled : !p.enabled;
    const out = await ps.setEnabled(user.id, slug, next);
    return json({ plugin: publicPlugin(out!, siteUrl()) });
  } catch (e) {
    return pluginErr(e);
  }
}
