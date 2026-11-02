import { currentUser, json, sameOrigin, store, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** API keys for the CLI, MCP clients and OpenAI-compatible tools. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ keys: await store().listKeys(user.id) });
}

export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  const body = (await req.json().catch(() => ({}))) as { label?: string };
  const key = await store().createKey(user.id, typeof body.label === "string" ? body.label : "api key");
  return json({ key, note: "Shown once. Use it as: Authorization: Bearer <key>" }, 201);
}

export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: "cross-site request refused" }, 403);
  const id = new URL(req.url).searchParams.get("id") ?? "";
  const gone = await store().revokeKeyById(user.id, id);
  return json({ ok: gone }, gone ? 200 : 404);
}
