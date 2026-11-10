import { SRC } from "@/lib/chat/push";
import { bad, pushErr } from "@/lib/chat/push-http";
import { currentUser, env, pushStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/hosting/pushes/<id>/files/<n>: one file of a push, for the node
 * that publishes it. Inline files come from the push row; media is streamed
 * from our own result route (the same one the chat played it from), so
 * nothing is staged on c0mpute.com.
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string; n: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id, n } = await params;
  let file;
  try {
    const p = await pushStore().pushById(user.id, id);
    if (!p) return bad("no such push on this account", 404);
    file = p.files[Number(n)];
  } catch (e) {
    return pushErr(e);
  }
  if (!/^\d{1,3}$/.test(n) || !file) return bad("no such file", 404);
  if (file.text !== undefined) {
    return new Response(file.text, { headers: { "content-type": `${file.type}; charset=utf-8`, "cache-control": "no-store" } });
  }
  if (!file.src || !SRC.test(file.src)) return bad("no such file", 404);
  const self = `http://127.0.0.1:${env("PORT") || "3000"}${file.src}`;
  let upstream: Response;
  try {
    upstream = await fetch(self, {
      headers: { authorization: req.headers.get("authorization") ?? "", cookie: req.headers.get("cookie") ?? "" },
      signal: AbortSignal.timeout(120_000),
    });
  } catch (e) {
    return bad(`could not read ${file.path}: ${e instanceof Error ? e.message : e}`, 502);
  }
  const type = (upstream.headers.get("content-type") ?? "").split(";")[0].trim();
  if (upstream.status !== 200 || type.includes("json")) {
    await upstream.body?.cancel().catch(() => {});
    const code = upstream.status === 202 ? 409 : upstream.status === 200 ? 502 : upstream.status;
    return bad(upstream.status === 202 ? `${file.path} is still rendering` : `${file.path}: the network answered ${upstream.status}`, code);
  }
  const len = upstream.headers.get("content-length");
  return new Response(upstream.body, {
    headers: { "content-type": type || file.type, "cache-control": "no-store", ...(len ? { "content-length": len } : {}) },
  });
}
