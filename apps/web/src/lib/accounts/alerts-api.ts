// The account's fleet alert settings, as every surface sees them (API, CLI,
// MCP, /account): settings, open incidents and recent notices; saving a new
// address sends its confirmation link; a test notice.

import { patchSettings, type AlertSettings } from "./alerts";
import { alertStore, channels, isAdminAccount, sendVerifyEmail, smsConfigured } from "./alerts-runner";

export async function alertsView(userId: string) {
  const store = alertStore();
  const [settings, states, log, admin] = await Promise.all([store.get(userId), store.states(userId), store.history(userId, 30), isAdminAccount(userId).catch(() => false)]);
  const active = [...states.values()].filter((s) => s.active).map((s) => ({ node: s.node, kind: s.kind, since: s.since, detail: s.detail, notified: s.notified }));
  return { settings: { ...settings, sms_available: admin && smsConfigured() }, active, log };
}

/** Apply a patch; returns the new view plus a notice, or an error message. */
export async function saveAlerts(userId: string, patch: Record<string, unknown>): Promise<{ error: string } | (Awaited<ReturnType<typeof alertsView>> & { notice: string | null })> {
  const store = alertStore();
  const cur = await store.get(userId);
  const p = patchSettings(cur, patch);
  if (typeof p === "string") return { error: p };
  if (p.next.sms && !cur.sms && !((await isAdminAccount(userId).catch(() => false)) && smsConfigured())) {
    return { error: "sms: only Profullstack's own admin accounts can use the fleet alert line; email works for every account" };
  }
  const { verifyToken } = await store.save(userId, p.next, p.emailChanged);
  let notice: string | null = null;
  if (verifyToken && p.next.email) {
    try {
      await sendVerifyEmail(p.next.email, verifyToken);
      notice = `We sent a confirmation link to ${p.next.email}. Alerts go there once you open it (within 48 hours).`;
    } catch (e) {
      console.error("fleet alerts: confirmation email failed", e);
      notice = `Saved, but the confirmation email to ${p.next.email} could not be sent right now. Save the address again to retry.`;
    }
  } else if (p.emailChanged && !p.next.email) notice = "Alert email removed.";
  return { ...(await alertsView(userId)), notice };
}

/** A test notice on every channel the account has. */
export async function testAlert(userId: string, s?: AlertSettings): Promise<{ ok: boolean; detail: string }> {
  const settings = s ?? (await alertStore().get(userId));
  const sent: string[] = [];
  if (settings.email && settings.email_verified) {
    await channels.email(settings.email, "c0mpute fleet alerts: test", [
      "This is a test of your c0mpute fleet alerts. Real ones look like:",
      "ALERT  node a1b2c3d4 (rack-a): offline. not heard from for 16 min",
      "OK  node a1b2c3d4 (rack-a): back online. online",
    ]);
    sent.push(`email to ${settings.email}`);
  }
  if (settings.sms && channels.sms && (await channels.sms(userId, "c0mpute fleet alerts: test notice", "info"))) sent.push("sms");
  if (!sent.length) return { ok: false, detail: settings.email ? `confirm ${settings.email} first (open the link we emailed)` : "set an alert email first" };
  await alertStore().log(userId, { node: "*", kind: "test", event: "test", detail: "test notice" }, sent.map((x) => x.split(" ")[0]), true);
  return { ok: true, detail: `sent: ${sent.join(", ")}` };
}
