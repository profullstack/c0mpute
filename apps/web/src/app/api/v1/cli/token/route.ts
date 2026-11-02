import { clientIp, json, rateLimit, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** `c0mpute chat login`, step 3: poll until the person approves, then receive an API key once. */
export async function POST(req: Request) {
  if (!rateLimit(`poll:${clientIp(req)}`, 1200).ok) return json({ error: "slow_down" }, 429);
  const body = (await req.json().catch(() => ({}))) as { device_code?: string };
  if (typeof body.device_code !== "string") return json({ error: "device_code is required" }, 400);
  const r = await store().pollDevice(body.device_code);
  if (r.status === "ok") return json({ key: r.key });
  if (r.status === "pending") return json({ error: "authorization_pending" }, 428);
  return json({ error: "expired_token" }, 410);
}
