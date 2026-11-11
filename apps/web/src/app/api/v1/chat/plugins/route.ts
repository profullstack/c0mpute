import { addPlugin, catalogFor, connectLink, pluginErr } from "@/lib/chat/plugins";
import { publicPlugin } from "@/lib/chat/plugin-store";
import { currentUser, json, pluginStore, rateLimit, sameOrigin, siteUrl, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/chat/plugins: this account's chat plugins and the catalog. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    const plugins = await pluginStore().list(user.id);
    const added = new Set(plugins.map((p) => p.url));
    return json({
      plugins: plugins.map((p) => publicPlugin(p, siteUrl())),
      catalog: catalogFor(siteUrl()).map((c) => ({ ...c, added: added.has(c.url) })),
    });
  } catch (e) {
    return pluginErr(e);
  }
}

/**
 * POST /api/v1/chat/plugins {url} or {slug: <catalog name>}: add a remote MCP
 * server. Answers 201 with the plugin; when it needs a sign-in, `connect_url`
 * is a one-time link (10 minutes) that works in any browser.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  if (!rateLimit(`plugins:${user.id}`, 60).ok) return json({ error: { message: "too many plugin changes this hour", code: 429 } }, 429);
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  const target = typeof body?.url === "string" ? body.url : typeof body?.slug === "string" ? body.slug : "";
  if (!target) return json({ error: { message: "url (an https MCP server) or slug (a catalog name) is required", code: 400 } }, 400);
  try {
    const p = await addPlugin(user.id, target);
    const pub = publicPlugin(p, siteUrl());
    return json({ plugin: pub, connect_url: pub.connected ? null : connectLink(user.id, p.slug) }, 201);
  } catch (e) {
    return pluginErr(e);
  }
}
