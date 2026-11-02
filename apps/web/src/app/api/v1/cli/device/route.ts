import { clientIp, json, rateLimit, siteUrl, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** `c0mpute chat login`, step 1: a device code for the CLI and a short code for the person. */
export async function POST(req: Request) {
  if (!rateLimit(`device:${clientIp(req)}`, 30).ok) return json({ error: "slow_down" }, 429);
  const body = (await req.json().catch(() => ({}))) as { label?: string };
  const label = typeof body.label === "string" && body.label ? body.label : "c0mpute cli";
  const d = await store().startDevice(label);
  const verification_uri = `${siteUrl()}/cli/approve`;
  return json({
    ...d,
    verification_uri,
    verification_uri_complete: `${verification_uri}?code=${encodeURIComponent(d.user_code)}`,
    interval: 3,
  });
}
