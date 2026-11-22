import { readWebhook, verifyWebhook } from "@profullstack/coinpay-onboard";
import { env } from "@/lib/chat/server";
import { gallery } from "@/lib/gallery/server";

export const dynamic = "force-dynamic";
export const maxDuration = 60;

const j = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });

/**
 * POST /api/v1/gallery/webhook: CoinPay events for the c0mpute.com business.
 * Unsigned or wrongly signed calls get 401 and change nothing
 * (X-CoinPay-Signature: t=<unix>,v1=<hmac-sha256 of "t.body">, 5-minute window).
 */
export async function POST(req: Request) {
  const secret = env("COINPAY_WEBHOOK_SECRET");
  if (!secret) return j({ error: "webhook not configured" }, 503);
  const raw = await req.text();
  if (!verifyWebhook(raw, req.headers.get("x-coinpay-signature"), secret)) return j({ error: "invalid signature" }, 401);
  let payload: unknown;
  try {
    payload = JSON.parse(raw);
  } catch {
    return j({ error: "invalid json" }, 400);
  }
  const ev = readWebhook(payload);
  try {
    const r = await gallery().onPaid(ev);
    return j({ received: true, action: r.action, listing: r.listing?.id ?? null });
  } catch (e) {
    console.error("gallery webhook failed", e);
    // 500 so CoinPay retries.
    return j({ error: "could not settle; will retry" }, 500);
  }
}

export function GET() {
  return j({ error: "POST only" }, 405);
}
