import { cleanTags } from "@/lib/accounts/store";
import { LinkError } from "@/lib/accounts/link";
import { currentLinker, fleetService, linkService } from "@/lib/accounts/server";
import { clientIp, json, rateLimit, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * `c0mpute node link`, step 2. Same Bearer as step 1. Body {challenge_id,
 * peer_id, signature (base64 ed25519 over the step-1 message), info?, label?,
 * tags?}. On success the node is verified to this account: it shows in the
 * fleet, earns to it, and accepts fleet commands signed with `fleet_key`,
 * which the node pins.
 */
export async function POST(req: Request) {
  if (!rateLimit(`link:${clientIp(req)}`, 600).ok) return json({ error: { message: "slow down", code: 429 } }, 429);
  const linker = await currentLinker(req);
  if (!linker) return unauthorized();
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const tags = body.tags === undefined ? [] : cleanTags(body.tags);
  if (!tags) return json({ error: { message: "tags: short lowercase words", code: 400 } }, 400);
  try {
    const r = await linkService().complete(linker, { ...body, tags });
    return json({ ...r, fleet_key: await fleetService().publicKey() }, 201);
  } catch (e) {
    if (e instanceof LinkError) return json({ error: { message: e.message, code: e.status } }, e.status);
    console.error("node link", e);
    return json({ error: { message: e instanceof Error ? e.message : "could not link the node", code: 409 } }, 409);
  }
}
