// Wiring for fleet alerts: the store on SQLite Cloud, email through Resend
// (with the network cross-promo block on alert mail, none on the address
// confirmation), SMS through smshub's fleet alert line for Profullstack admin
// accounts, and the schedule (startFleetAlerts, from instrumentation.ts).
// No "server-only" import, so instrumentation.ts can load it.

import { randomBytes } from "node:crypto";
import { promoBlock } from "@profullstack/cross-promo";
import { balanceFor, cloudSql, env, siteUrl } from "../chat/server";
import { earningsStore } from "../earnings/recorder";
import { resendMailer, type Mailer } from "../gallery/email";
import { getNodesPayload, type NodesPayload } from "../nodes-core";
import { createAlertStore, evaluateAll, type AlertStore, type Channels, type EvalDeps } from "./alerts";
import { createFleetService, type FleetService } from "./fleet";
import { earningsTrend, fleetView } from "./fleet-view";
import { createAccountStore, type AccountStore } from "./store";

let _alerts: AlertStore | null = null;
export const alertStore = () => (_alerts ??= createAlertStore(cloudSql));

// The scheduler's own stores: accounts/server.ts pulls in WebAuthn and
// "server-only". The fleet service here only reads (state, update results);
// it never signs, so it needs no key.
let _accounts: AccountStore | null = null;
let _fleet: FleetService | null = null;
const accounts = () => (_accounts ??= createAccountStore(cloudSql));
const fleetRead = () => (_fleet ??= createFleetService({ sql: cloudSql, accounts: accounts(), site: siteUrl() }));

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

/** A plain message as text + HTML; `promo` adds the network cross-promo block. */
export async function renderPlain(to: string, subject: string, lines: string[], promo: boolean) {
  let block = { text: "", html: "" };
  if (promo) {
    try {
      block = await promoBlock({ site: "c0mpute.com", audience: ["developers", "makers"], seed: to });
    } catch {
      // The catalog is optional; the alert is not.
    }
  }
  const text = [...lines, "", "c0mpute, the peer-to-peer compute network", block.text].join("\n").trim();
  const p = (s: string) => (s ? `<p style="margin:0 0 10px;line-height:1.5">${esc(s).replace(/(https:\/\/[^\s)`]+)/g, '<a href="$1">$1</a>')}</p>` : "");
  const html = `<div style="font-family:ui-sans-serif,system-ui,sans-serif;font-size:15px;color:#111;max-width:600px">
${lines.map(p).join("\n")}
<p style="margin:18px 0 0;color:#666;font-size:13px">c0mpute, the peer-to-peer compute network</p>
${block.html}
</div>`;
  return { to, subject, text, html };
}

let mailer: Mailer | null = null;
const mail = (): Mailer => (mailer ??= resendMailer);
/** Tests swap the mailer. */
export function setAlertMailer(m: Mailer | null) {
  mailer = m;
}

export async function sendAlertEmail(to: string, subject: string, lines: string[]) {
  await mail()(await renderPlain(to, subject, lines, true));
}

/** The double opt-in for an alert address: clean, no promo. */
export async function sendVerifyEmail(to: string, token: string) {
  const link = `${siteUrl()}/api/v1/fleet/alerts/verify?token=${encodeURIComponent(token)}`;
  await mail()(
    await renderPlain(
      to,
      "Confirm your c0mpute fleet alert email",
      [
        "Someone (hopefully you) asked c0mpute.com to send fleet alerts to this address: a node offline, earnings dropping, an update failing, a disk running low.",
        `Confirm: ${link}`,
        "The link works for 48 hours. If this was not you, ignore this email and nothing will be sent.",
      ],
      false,
    ),
  );
}

/** Profullstack's own accounts: CoinPay subject in C0MPUTE_ADMIN_SUBS, or an Infernet admin. */
export async function isAdminAccount(userId: string): Promise<boolean> {
  const r = (await cloudSql.all(`SELECT coinpay_sub FROM chat_users WHERE id = ?`, [userId]))[0];
  const sub = (r?.coinpay_sub as string) ?? null;
  if (!sub) return false;
  if (
    env("C0MPUTE_ADMIN_SUBS")
      .split(",")
      .map((s) => s.trim())
      .includes(sub)
  )
    return true;
  const b = (await balanceFor(sub)) as { admin?: unknown } | null;
  return b?.admin === true;
}

/** SMS needs the fleet alert key on this server, and an admin account. */
export const smsConfigured = () => !!env("SMSHUB_ALERT_API_KEY");

/** The fleet alert line (smshub /api/v1/alerts): texts, or calls, Profullstack's on-call phone. */
export async function sendSms(userId: string, message: string, severity: "warning" | "info"): Promise<boolean> {
  const key = env("SMSHUB_ALERT_API_KEY");
  if (!key || !(await isAdminAccount(userId))) return false;
  const res = await fetch(`${env("SMSHUB_URL") || "https://smshub.dev"}/api/v1/alerts`, {
    method: "POST",
    headers: { "x-api-key": key, "content-type": "application/json" },
    body: JSON.stringify({ message: message.slice(0, 300), severity }),
    signal: AbortSignal.timeout(30_000),
  });
  if (!res.ok) console.error("fleet alerts: smshub", res.status, (await res.text().catch(() => "")).slice(0, 200));
  return res.ok;
}

export const channels: Channels = { email: sendAlertEmail, sms: sendSms };

function deps(board: NodesPayload | null): EvalDeps {
  const earnings = earningsStore();
  return {
    store: alertStore(),
    fleet: async (userId) => {
      const v = await fleetView(userId, { accounts: accounts(), fleet: fleetRead(), earnings, board });
      return { nodes: v.nodes, boardOk: v.boardOk };
    },
    earnings: (nodes) => earningsTrend(earnings, nodes),
    updates: (userId) => fleetRead().lastUpdates(userId),
    channels,
    site: siteUrl(),
  };
}

/** One pass over every account with alerts on (reads the node board once). */
export async function runAlertPass(): Promise<number> {
  const board = await getNodesPayload().catch(() => null);
  return evaluateAll(deps(board));
}

let started = false;
const HOLDER = `${process.pid}-${randomBytes(4).toString("hex")}`;
/** Every minute for the life of the server; one server at a time holds the lease. */
export function startFleetAlerts(everyMs = 60_000) {
  if (started || env("C0MPUTE_FLEET_ALERTS") === "0" || !env("SQLITE_CLOUD_URL")) return;
  started = true;
  let running = false;
  const tick = async () => {
    if (running) return;
    running = true;
    try {
      if (await alertStore().lease(HOLDER, everyMs * 3)) await runAlertPass();
    } catch (e) {
      console.error("fleet alerts: pass failed", e);
    } finally {
      running = false;
    }
  };
  // After the earnings recorder's first pass (15 s), so fresh numbers count.
  const first = setTimeout(tick, 45_000);
  (first as { unref?: () => void }).unref?.();
  const t = setInterval(tick, everyMs);
  (t as { unref?: () => void }).unref?.();
}
