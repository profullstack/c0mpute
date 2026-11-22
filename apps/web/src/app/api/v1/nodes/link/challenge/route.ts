import { LinkError } from "@/lib/accounts/link";
import { currentLinker, linkService } from "@/lib/accounts/server";
import { clientIp, json, rateLimit, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * `c0mpute node link`, step 1. Bearer: an API key (c0k_) or a fleet enrollment
 * token (c0e_). Body {peer_id}. Returns the exact message the node must sign
 * with its identity key, and a one-shot challenge id (5 minutes).
 */
export async function POST(req: Request) {
  if (!rateLimit(`link:${clientIp(req)}`, 600).ok) return json({ error: { message: "slow down", code: 429 } }, 429);
  const linker = await currentLinker(req);
  if (!linker) return unauthorized();
  const body = (await req.json().catch(() => ({}))) as { peer_id?: unknown };
  try {
    return json(await linkService().challenge(linker, body.peer_id));
  } catch (e) {
    if (e instanceof LinkError) return json({ error: { message: e.message, code: e.status } }, e.status);
    console.error("link challenge", e);
    return json({ error: { message: "could not start the link; try again", code: 503 } }, 503);
  }
}
