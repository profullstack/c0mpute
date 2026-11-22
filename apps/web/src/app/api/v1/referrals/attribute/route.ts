import { bad } from "@/lib/chat/push-http";
import { currentUser } from "@/lib/chat/server";
import { forwardedIp, referralUpstream, relay } from "@/lib/referrals";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/referrals/attribute { peer_id, code, ts, sig }
 *
 * Called by `c0mpute worker register --ref <code>` (and the worker on start
 * while a saved code is unanswered). `sig` is the node's ed25519 signature,
 * made with its libp2p identity key, over
 *   c0mpute-referral:v1:<peer_id>:<code>:<ts>
 * Infernet verifies it, so only the node itself can claim a referral.
 *
 * When the node sends its chat key (Bearer c0k_...), the operator's account
 * goes along too, so Infernet can refuse a self-referral by account; the
 * node's address goes along for the same-network check.
 */
export async function POST(req: Request) {
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body) return bad("JSON body required: { peer_id, code, ts, sig }");
  const pick = (v: unknown, max: number) => (typeof v === "string" ? v.slice(0, max) : typeof v === "number" ? v : undefined);
  const operator = /^Bearer\s/i.test(req.headers.get("authorization") ?? "") ? await currentUser(req) : null;
  try {
    return relay(
      await referralUpstream(operator?.coinpay_sub ?? null, forwardedIp(req))("/api/v1/referrals/attribute", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ peer_id: pick(body.peer_id, 80), code: pick(body.code, 200), ts: pick(body.ts, 20), sig: pick(body.sig, 200) }),
        signal: AbortSignal.timeout(10_000),
      }),
    );
  } catch {
    return bad("could not record the referral right now", 503);
  }
}
