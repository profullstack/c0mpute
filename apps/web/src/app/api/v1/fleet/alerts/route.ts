import { alertsView, saveAlerts, testAlert } from "@/lib/accounts/alerts-api";
import { currentUser, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/fleet/alerts: the account's alert settings, open incidents and
 * recent notices. Same as `c0mpute fleet alerts get` and the MCP tool
 * fleet_alerts_get.
 *
 * Settings: email (confirmed by a link before anything is sent), sms (the
 * fleet alert line, Profullstack admin accounts only), offline_minutes (15),
 * earnings_drop_pct (50: the last 24h against the trailing 7-day daily
 * average), earnings_min_usd (0.05: smaller averages are not compared),
 * disk_min_gb (10) and disk_min_pct (5) (0 turns one off), cooldown_minutes
 * (60), kinds {offline, earnings, update, disk}.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    return json(await alertsView(user.id));
  } catch (e) {
    console.error("fleet alerts", e);
    return json({ error: { message: "could not read your alert settings right now", code: 503 } }, 503);
  }
}

/** PUT /api/v1/fleet/alerts {any of the settings}: change them. A new email gets a confirmation link. */
export async function PUT(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body || typeof body !== "object" || Array.isArray(body)) return json({ error: { message: "send a JSON object of settings", code: 400 } }, 400);
  if ("email" in body && !rateLimit(`alert-email:${user.id}`, 10).ok) return json({ error: { message: "too many address changes; try again in an hour", code: 429 } }, 429);
  const r = await saveAlerts(user.id, body);
  if ("error" in r) return json({ error: { message: r.error, code: 400 } }, 400);
  return json(r);
}

/** POST /api/v1/fleet/alerts {test: true}: send a test notice to every channel the account has. */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const body = (await req.json().catch(() => ({}))) as { test?: unknown };
  if (body.test !== true) return json({ error: { message: "send {test: true}", code: 400 } }, 400);
  if (!rateLimit(`alert-test:${user.id}`, 6).ok) return json({ error: { message: "at most 6 tests an hour", code: 429 } }, 429);
  try {
    const r = await testAlert(user.id);
    return json(r, r.ok ? 200 : 409);
  } catch (e) {
    console.error("fleet alerts: test", e);
    return json({ error: { message: "could not send the test right now", code: 503 } }, 503);
  }
}
