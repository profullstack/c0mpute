import { ShareError } from "@/lib/chat/share-store";
import { currentUser, json, sameOrigin, shareStore, siteUrl, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const shareErr = (e: unknown) => {
  if (e instanceof ShareError) return json({ error: { message: e.message, code: e.status } }, e.status);
  console.error("chat shares", e);
  return json({ error: { message: "sharing failed; try again", code: 500 } }, 500);
};

/**
 * POST /api/v1/chat/shares { convo_id, title?, messages: [{ role, content, sources? }] }
 * Publishes a read-only copy of a chat at /share/<id> (anyone with the link can
 * read it, no sign-in). Sharing the same chat again updates the same link.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const body = await req.json().catch(() => null);
  try {
    const { id, created } = await shareStore().put(user.id, body);
    return json({ id, url: `${siteUrl()}/share/${id}`, created }, created ? 201 : 200);
  } catch (e) {
    return shareErr(e);
  }
}

/** GET /api/v1/chat/shares[?convo_id=]: this account's live shared chats (or the one for a chat). */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const convo = new URL(req.url).searchParams.get("convo_id");
  try {
    if (convo) {
      const id = await shareStore().forConvo(user.id, convo);
      return json({ id, url: id ? `${siteUrl()}/share/${id}` : null });
    }
    const data = (await shareStore().list(user.id)).map((s) => ({ ...s, url: `${siteUrl()}/share/${s.id}` }));
    return json({ data });
  } catch (e) {
    return shareErr(e);
  }
}
