import { alertStore } from "@/lib/accounts/alerts-runner";
import { clientIp, rateLimit, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/fleet/alerts/verify?token=c0v_...: the link in the confirmation email. */
export async function GET(req: Request) {
  const back = (q: string) => Response.redirect(`${siteUrl()}/account?alerts=${q}#alerts`, 303);
  if (!rateLimit(`alert-verify:${clientIp(req)}`, 30).ok) return back("slow-down");
  const token = new URL(req.url).searchParams.get("token") ?? "";
  try {
    return back((await alertStore().verify(token)) ? "confirmed" : "link-expired");
  } catch (e) {
    console.error("fleet alerts: verify", e);
    return back("error");
  }
}
