// Fleet alerts per account: a node offline for N minutes, the fleet's earnings
// dropping (last 24h against the trailing 7-day daily average), an update that
// failed, a disk running low. Email to an address the operator confirmed; SMS
// through the fleet `alert` line (smshub) only for Profullstack's own admin
// accounts, since that line rings Profullstack's on-call phone.
//
// Evaluated on a server-side schedule (startFleetAlerts, from
// instrumentation.ts). Each (account, node, kind) is one incident: it sends one
// alert when it opens and one recovery notice when it clears. A new incident
// for the same thing within `cooldown_minutes` of a recovery is held back, and
// sent only if it is still open when the cooldown ends, so a flapping node
// does not flood the inbox. Everything a tick sends to one account goes in one
// message.
//
// No "server-only" import: instrumentation.ts loads the scheduler.

import { createHash, randomBytes } from "node:crypto";
import type { Sql } from "../chat/store";
import type { FleetNode } from "./fleet";

export const ALERT_KINDS = ["offline", "earnings", "update", "disk"] as const;
export type AlertKind = (typeof ALERT_KINDS)[number];

export type AlertSettings = {
  email: string | null;
  email_verified: boolean;
  /** SMS through the fleet alert line (admin accounts only; ignored otherwise). */
  sms: boolean;
  offline_minutes: number;
  earnings_drop_pct: number;
  earnings_min_usd: number;
  disk_min_gb: number;
  disk_min_pct: number;
  cooldown_minutes: number;
  kinds: Record<AlertKind, boolean>;
};

export const DEFAULT_ALERTS: AlertSettings = {
  email: null,
  email_verified: false,
  sms: false,
  offline_minutes: 15,
  earnings_drop_pct: 50,
  earnings_min_usd: 0.05,
  disk_min_gb: 10,
  disk_min_pct: 5,
  cooldown_minutes: 60,
  kinds: { offline: true, earnings: true, update: true, disk: true },
};

/** An update that failed this long ago no longer counts as open. */
export const UPDATE_WINDOW_MS = 7 * 86_400_000;
/** Disk alerts clear only above the threshold plus this margin, so a disk at the line does not flap. */
export const DISK_HYSTERESIS = 1.1;
export const VERIFY_TTL_MS = 48 * 3_600_000;

const EMAIL_RE = /^[^\s@<>"',;]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)+$/;
export const validEmail = (e: string) => e.length <= 254 && EMAIL_RE.test(e);

const LIMITS: Record<string, [number, number]> = {
  offline_minutes: [5, 1440],
  earnings_drop_pct: [10, 100],
  earnings_min_usd: [0, 1000],
  disk_min_gb: [0, 100_000],
  disk_min_pct: [0, 90],
  cooldown_minutes: [0, 1440],
};

/** Merge a settings patch (API/CLI/MCP input) into the current settings, or say what is wrong. */
export function patchSettings(cur: AlertSettings, patch: Record<string, unknown>): { next: AlertSettings; emailChanged: boolean } | string {
  const next: AlertSettings = { ...cur, kinds: { ...cur.kinds } };
  let emailChanged = false;
  if ("email" in patch) {
    const e = patch.email;
    if (e === null || e === "") {
      emailChanged = cur.email !== null;
      next.email = null;
      next.email_verified = false;
    } else if (typeof e === "string" && validEmail(e.trim())) {
      const v = e.trim();
      if (v.toLowerCase() !== (cur.email ?? "").toLowerCase()) {
        next.email = v;
        next.email_verified = false;
        emailChanged = true;
      }
    } else return "email: an email address, or null to remove it";
  }
  for (const [k, [lo, hi]] of Object.entries(LIMITS)) {
    if (!(k in patch)) continue;
    const v = Number(patch[k]);
    if (!Number.isFinite(v) || v < lo || v > hi) return `${k}: a number from ${lo} to ${hi}`;
    (next as Record<string, unknown>)[k] = k.endsWith("_minutes") || k === "earnings_drop_pct" ? Math.round(v) : v;
  }
  if ("sms" in patch) {
    if (typeof patch.sms !== "boolean") return "sms: true or false";
    next.sms = patch.sms;
  }
  if ("kinds" in patch) {
    const k = patch.kinds;
    if (!k || typeof k !== "object" || Array.isArray(k)) return "kinds: {offline, earnings, update, disk: true|false}";
    for (const [name, on] of Object.entries(k as Record<string, unknown>)) {
      if (!(ALERT_KINDS as readonly string[]).includes(name) || typeof on !== "boolean") return "kinds: {offline, earnings, update, disk: true|false}";
      next.kinds[name as AlertKind] = on;
    }
  }
  return { next, emailChanged };
}

// ── evaluation (pure) ─────────────────────────────────────────────────────────

/** One check of one thing: is it wrong right now, and in words. Node "*" is the whole fleet. */
export type Finding = { node: string; label: string | null; kind: AlertKind; firing: boolean; detail: string };

export type AlertState = {
  node: string;
  kind: AlertKind;
  active: boolean;
  notified: boolean;
  since: string | null;
  detail: string;
  last_alert_at: string | null;
  last_recovered_at: string | null;
};

export const stateKey = (node: string, kind: string) => `${node}|${kind}`;

const gb = (b: number) => `${(b / 1e9).toFixed(1)} GB`;
const mins = (s: number) => (s >= 5400 ? `${Math.round(s / 3600)} h` : `${Math.round(s / 60)} min`);
const usd = (n: number) => `$${n.toFixed(4)}`;

/**
 * What is wrong with the fleet right now. `boardOk` false (the node board
 * could not be read) skips the offline check rather than calling every node
 * offline. `active` holds the incidents open now (for hysteresis).
 */
export function findings(o: {
  settings: AlertSettings;
  nodes: FleetNode[];
  boardOk: boolean;
  earnings: { h24: number; avg7: number } | null;
  updates: Map<string, { status: string; done_at: string | null; result: string | null }>;
  active: Set<string>;
  now: number;
}): Finding[] {
  const s = o.settings;
  const out: Finding[] = [];
  for (const n of o.nodes) {
    const base = { node: n.node, label: n.label };
    if (o.boardOk) {
      // Silent for: the board's last word, and the node's own last poll; the fresher one counts.
      const ages: number[] = [];
      if (n.online) ages.push(0);
      else if (n.last_seen_s != null) ages.push(n.last_seen_s);
      if (n.state_at) ages.push(Math.max(0, (o.now - Date.parse(n.state_at)) / 1000));
      if (!ages.length) ages.push(Math.max(0, (o.now - Date.parse(n.linked_at)) / 1000));
      const age = Math.min(...ages);
      const firing = age > s.offline_minutes * 60;
      out.push({ ...base, kind: "offline", firing, detail: firing ? `not heard from for ${mins(age)}` : "online" });
    }
    const disk = n.node_state?.disk ?? n.storage;
    if (disk && disk.total_bytes > 0) {
      const pct = (disk.free_bytes / disk.total_bytes) * 100;
      const wasActive = o.active.has(stateKey(n.node, "disk"));
      const m = wasActive ? DISK_HYSTERESIS : 1;
      const lowGb = s.disk_min_gb > 0 && disk.free_bytes < s.disk_min_gb * 1e9 * m;
      const lowPct = s.disk_min_pct > 0 && pct < s.disk_min_pct * m;
      const firing = lowGb || lowPct;
      out.push({ ...base, kind: "disk", firing, detail: `${gb(disk.free_bytes)} free of ${gb(disk.total_bytes)} (${pct.toFixed(1)}%)` });
    }
    const u = o.updates.get(n.node);
    const recentFail = u?.status === "failed" && !!u.done_at && o.now - Date.parse(u.done_at) < UPDATE_WINDOW_MS;
    const nodeErr = n.node_state?.update_error;
    const firing = recentFail || !!nodeErr;
    out.push({
      ...base,
      kind: "update",
      firing,
      detail: nodeErr ? `update to ${nodeErr.version} failed: ${nodeErr.error}` : recentFail ? (u?.result ?? "update failed").slice(0, 300) : `on ${n.node_state?.version ?? n.version ?? "?"}`,
    });
  }
  if (o.earnings && o.nodes.length) {
    const { h24, avg7 } = o.earnings;
    const floor = avg7 * (1 - s.earnings_drop_pct / 100);
    const firing = avg7 >= s.earnings_min_usd && h24 < floor;
    const drop = avg7 > 0 ? Math.round((1 - h24 / avg7) * 100) : 0;
    out.push({ node: "*", label: "fleet", kind: "earnings", firing, detail: `last 24h ${usd(h24)} vs 7-day daily average ${usd(avg7)}${drop > 0 ? ` (${drop}% down)` : ""}` });
  }
  return out;
}

export type Notice = { node: string; label: string | null; kind: AlertKind; event: "alert" | "recovery"; detail: string };

/**
 * Open, hold, notify and close incidents. Returns the notices to send now and
 * the state rows to write. Kinds turned off close their open incidents without
 * a notice; nodes no longer on the account do too. Kinds not checked this tick
 * (`evaluated`) are left as they are.
 */
export function transitions(o: {
  settings: AlertSettings;
  findings: Finding[];
  states: Map<string, AlertState>;
  evaluated: Set<AlertKind>;
  fleetNodes: Set<string>;
  now: number;
}): { notices: Notice[]; writes: AlertState[] } {
  const iso = new Date(o.now).toISOString();
  const cooled = (st: AlertState | undefined) => !st?.last_recovered_at || o.now - Date.parse(st.last_recovered_at) >= o.settings.cooldown_minutes * 60_000;
  const notices: Notice[] = [];
  const writes: AlertState[] = [];
  const seen = new Set<string>();
  for (const f of o.findings) {
    const key = stateKey(f.node, f.kind);
    seen.add(key);
    const st = o.states.get(key);
    const on = o.settings.kinds[f.kind];
    if (!on || !f.firing) {
      if (st?.active) {
        // The cooldown runs from the last recovery the operator was told about.
        writes.push({ ...st, active: false, notified: false, detail: f.detail, last_recovered_at: st.notified ? iso : st.last_recovered_at });
        if (on && st.notified) notices.push({ node: f.node, label: f.label, kind: f.kind, event: "recovery", detail: f.detail });
      }
      continue;
    }
    if (!st?.active) {
      const send = cooled(st);
      writes.push({
        node: f.node,
        kind: f.kind,
        active: true,
        notified: send,
        since: iso,
        detail: f.detail,
        last_alert_at: send ? iso : (st?.last_alert_at ?? null),
        last_recovered_at: st?.last_recovered_at ?? null,
      });
      if (send) notices.push({ node: f.node, label: f.label, kind: f.kind, event: "alert", detail: f.detail });
    } else if (!st.notified && cooled(st)) {
      writes.push({ ...st, notified: true, detail: f.detail, last_alert_at: iso });
      notices.push({ node: f.node, label: f.label, kind: f.kind, event: "alert", detail: f.detail });
    } else if (st.detail !== f.detail) {
      writes.push({ ...st, detail: f.detail });
    }
  }
  for (const [key, st] of o.states) {
    if (seen.has(key) || !st.active) continue;
    const gone = st.node !== "*" && !o.fleetNodes.has(st.node);
    if (gone || (o.evaluated.has(st.kind) && !o.settings.kinds[st.kind])) writes.push({ ...st, active: false, notified: false });
  }
  return { notices, writes };
}

const KIND_WORD: Record<AlertKind, [string, string]> = {
  offline: ["offline", "back online"],
  earnings: ["earnings dropped", "earnings recovered"],
  update: ["update failed", "update fixed"],
  disk: ["disk low", "disk OK"],
};

const who = (n: Notice) => (n.node === "*" ? "your fleet" : `node ${n.node}${n.label ? ` (${n.label})` : ""}`);

/** One message for everything a tick has for one account. */
export function composeNotice(notices: Notice[], site: string): { subject: string; lines: string[]; sms: string } {
  const alerts = notices.filter((n) => n.event === "alert");
  const recov = notices.filter((n) => n.event === "recovery");
  const first = notices[0];
  let subject: string;
  if (notices.length === 1) subject = `c0mpute ${first.event === "alert" ? "alert" : "recovered"}: ${who(first)} ${KIND_WORD[first.kind][first.event === "alert" ? 0 : 1]}`;
  else subject = `c0mpute fleet: ${[alerts.length ? `${alerts.length} alert${alerts.length > 1 ? "s" : ""}` : "", recov.length ? `${recov.length} recovered` : ""].filter(Boolean).join(", ")}`;
  const line = (n: Notice) => `${n.event === "alert" ? "ALERT" : "OK"}  ${who(n)}: ${KIND_WORD[n.kind][n.event === "alert" ? 0 : 1]}. ${n.detail}`;
  const lines = [...alerts.map(line), ...recov.map(line), "", `Your fleet: ${site}/dashboard/nodes`, `Alert settings (thresholds, kinds, email): ${site}/account#alerts or \`c0mpute fleet alerts set\``];
  const sms = `${subject}. ${notices.map((n) => `${who(n)}: ${n.detail}`).join("; ")}`.slice(0, 300);
  return { subject, lines, sms };
}

// ── store ─────────────────────────────────────────────────────────────────────

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS fleet_alert_settings (
     user_id           TEXT PRIMARY KEY,
     settings          TEXT NOT NULL,
     email             TEXT,
     email_verified_at TEXT,
     verify_hash       TEXT,
     verify_expires    TEXT,
     updated_at        TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_alert_settings_verify ON fleet_alert_settings(verify_hash)`,
  `CREATE TABLE IF NOT EXISTS fleet_alert_state (
     user_id           TEXT NOT NULL,
     node              TEXT NOT NULL,
     kind              TEXT NOT NULL,
     active            INTEGER NOT NULL DEFAULT 0,
     notified          INTEGER NOT NULL DEFAULT 0,
     since             TEXT,
     detail            TEXT NOT NULL DEFAULT '',
     last_alert_at     TEXT,
     last_recovered_at TEXT,
     PRIMARY KEY (user_id, node, kind)
   )`,
  `CREATE TABLE IF NOT EXISTS fleet_alert_log (
     id       TEXT PRIMARY KEY,
     user_id  TEXT NOT NULL,
     node     TEXT NOT NULL,
     kind     TEXT NOT NULL,
     event    TEXT NOT NULL,
     channels TEXT NOT NULL,
     detail   TEXT NOT NULL,
     ok       INTEGER NOT NULL,
     at       TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_alert_log_user ON fleet_alert_log(user_id, at)`,
  `CREATE TABLE IF NOT EXISTS fleet_alert_lease (
     id     INTEGER PRIMARY KEY CHECK (id = 1),
     holder TEXT NOT NULL,
     until  INTEGER NOT NULL
   )`,
];

const hashToken = (t: string) => createHash("sha256").update(t).digest("hex");

export function createAlertStore(sql: Sql, now: () => number = Date.now) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));
  const iso = (ms = now()) => new Date(ms).toISOString();

  const read = (r: Record<string, unknown> | undefined): AlertSettings => {
    if (!r) return { ...DEFAULT_ALERTS, kinds: { ...DEFAULT_ALERTS.kinds } };
    let saved: Partial<AlertSettings> = {};
    try {
      saved = JSON.parse(String(r.settings));
    } catch {}
    return {
      ...DEFAULT_ALERTS,
      ...saved,
      kinds: { ...DEFAULT_ALERTS.kinds, ...(saved.kinds ?? {}) },
      email: (r.email as string) ?? null,
      email_verified: !!r.email && !!r.email_verified_at,
    };
  };

  return {
    init,
    async get(userId: string): Promise<AlertSettings> {
      await init();
      return read((await sql.all(`SELECT * FROM fleet_alert_settings WHERE user_id = ?`, [userId]))[0]);
    },

    /**
     * Save settings. A new email is unconfirmed until its link is opened;
     * returns the one-time confirmation token to email then.
     */
    async save(userId: string, s: AlertSettings, emailChanged: boolean): Promise<{ verifyToken: string | null }> {
      await init();
      const { email, email_verified, ...rest } = s;
      void email_verified;
      const token = emailChanged && email ? "c0v_" + randomBytes(24).toString("base64url") : null;
      const have = (await sql.all(`SELECT user_id FROM fleet_alert_settings WHERE user_id = ?`, [userId]))[0];
      if (!have) {
        await sql.run(`INSERT OR IGNORE INTO fleet_alert_settings (user_id, settings, updated_at) VALUES (?, ?, ?)`, [userId, "{}", iso()]);
      }
      await sql.run(`UPDATE fleet_alert_settings SET settings = ?, updated_at = ? WHERE user_id = ?`, [JSON.stringify(rest), iso(), userId]);
      if (emailChanged) {
        await sql.run(`UPDATE fleet_alert_settings SET email = ?, email_verified_at = NULL, verify_hash = ?, verify_expires = ? WHERE user_id = ?`, [
          email,
          token ? hashToken(token) : null,
          token ? iso(now() + VERIFY_TTL_MS) : null,
          userId,
        ]);
      }
      return { verifyToken: token };
    },

    /** Open a confirmation link: the address it was sent to becomes the alert address. */
    async verify(token: string): Promise<{ user_id: string; email: string } | null> {
      await init();
      if (!token.startsWith("c0v_")) return null;
      const r = (await sql.all(`SELECT user_id, email, verify_expires FROM fleet_alert_settings WHERE verify_hash = ?`, [hashToken(token)]))[0];
      if (!r || !r.email || !r.verify_expires || Date.parse(String(r.verify_expires)) < now()) return null;
      await sql.run(`UPDATE fleet_alert_settings SET email_verified_at = ?, verify_hash = NULL, verify_expires = NULL WHERE user_id = ?`, [iso(), r.user_id]);
      return { user_id: String(r.user_id), email: String(r.email) };
    },

    /** Accounts with somewhere to send alerts: a confirmed email, or SMS on. */
    async recipients(): Promise<{ user_id: string; settings: AlertSettings }[]> {
      await init();
      const rows = await sql.all(`SELECT * FROM fleet_alert_settings WHERE (email IS NOT NULL AND email_verified_at IS NOT NULL) OR settings LIKE '%"sms":true%'`);
      return rows.map((r) => ({ user_id: String(r.user_id), settings: read(r) }));
    },

    async states(userId: string): Promise<Map<string, AlertState>> {
      await init();
      const out = new Map<string, AlertState>();
      for (const r of await sql.all(`SELECT * FROM fleet_alert_state WHERE user_id = ?`, [userId])) {
        out.set(stateKey(String(r.node), String(r.kind)), {
          node: String(r.node),
          kind: String(r.kind) as AlertKind,
          active: !!Number(r.active),
          notified: !!Number(r.notified),
          since: (r.since as string) ?? null,
          detail: String(r.detail ?? ""),
          last_alert_at: (r.last_alert_at as string) ?? null,
          last_recovered_at: (r.last_recovered_at as string) ?? null,
        });
      }
      return out;
    },

    async writeStates(userId: string, rows: AlertState[]) {
      await init();
      for (const s of rows) {
        await sql.run(
          `INSERT INTO fleet_alert_state (user_id, node, kind, active, notified, since, detail, last_alert_at, last_recovered_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id, node, kind) DO UPDATE SET active = excluded.active, notified = excluded.notified, since = excluded.since,
               detail = excluded.detail, last_alert_at = excluded.last_alert_at, last_recovered_at = excluded.last_recovered_at`,
          [userId, s.node, s.kind, s.active ? 1 : 0, s.notified ? 1 : 0, s.since, s.detail.slice(0, 500), s.last_alert_at, s.last_recovered_at],
        );
      }
    },

    async log(userId: string, n: { node: string; kind: string; event: string; detail: string }, channels: string[], ok: boolean) {
      await init();
      await sql.run(`INSERT INTO fleet_alert_log (id, user_id, node, kind, event, channels, detail, ok, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`, [
        "al_" + randomBytes(10).toString("hex"),
        userId,
        n.node,
        n.kind,
        n.event,
        channels.join(","),
        n.detail.slice(0, 500),
        ok ? 1 : 0,
        iso(),
      ]);
    },

    async history(userId: string, limit = 50) {
      await init();
      return (await sql.all(`SELECT node, kind, event, channels, detail, ok, at FROM fleet_alert_log WHERE user_id = ? ORDER BY at DESC LIMIT ?`, [userId, Math.min(Math.max(limit, 1), 200)])).map((r) => ({
        node: String(r.node),
        kind: String(r.kind),
        event: String(r.event),
        channels: String(r.channels).split(",").filter(Boolean),
        detail: String(r.detail),
        ok: !!Number(r.ok),
        at: String(r.at),
      }));
    },

    /** Delete an account's alert data (account deletion, test cleanup). */
    async forget(userId: string) {
      await init();
      for (const t of ["fleet_alert_settings", "fleet_alert_state", "fleet_alert_log"]) await sql.run(`DELETE FROM ${t} WHERE user_id = ?`, [userId]);
    },

    /** One evaluator at a time across server instances (a deploy overlaps two). */
    async lease(holder: string, ms: number): Promise<boolean> {
      await init();
      const t = now();
      await sql.run(`INSERT OR IGNORE INTO fleet_alert_lease (id, holder, until) VALUES (1, ?, 0)`, [holder]);
      await sql.run(`UPDATE fleet_alert_lease SET holder = ?, until = ? WHERE id = 1 AND (until < ? OR holder = ?)`, [holder, t + ms, t, holder]);
      const r = (await sql.all(`SELECT holder FROM fleet_alert_lease WHERE id = 1`))[0];
      return r?.holder === holder;
    },
  };
}

export type AlertStore = ReturnType<typeof createAlertStore>;

// ── one pass ──────────────────────────────────────────────────────────────────

export type Channels = {
  /** Send an email; throws on failure. */
  email(to: string, subject: string, lines: string[]): Promise<void>;
  /** Send a text/call through the fleet alert line, or null when this account may not. */
  sms: ((userId: string, message: string, severity: "warning" | "info") => Promise<boolean>) | null;
};

export type EvalDeps = {
  store: AlertStore;
  fleet: (userId: string) => Promise<{ nodes: FleetNode[]; boardOk: boolean }>;
  earnings: (nodes: string[]) => Promise<{ h24: number; avg7: number }>;
  updates: (userId: string) => Promise<Map<string, { status: string; done_at: string | null; result: string | null }>>;
  channels: Channels;
  site: string;
  now?: () => number;
};

/** Evaluate one account and send what it has. Returns the notices sent. */
export async function evaluateAccount(userId: string, settings: AlertSettings, d: EvalDeps): Promise<Notice[]> {
  const now = (d.now ?? Date.now)();
  const [{ nodes, boardOk }, states, updates] = await Promise.all([d.fleet(userId), d.store.states(userId), d.updates(userId)]);
  const earnings = settings.kinds.earnings && nodes.length ? await d.earnings(nodes.map((n) => n.node)).catch(() => null) : null;
  const active = new Set([...states].filter(([, s]) => s.active).map(([k]) => k));
  const f = findings({ settings, nodes, boardOk, earnings, updates, active, now });
  const evaluated = new Set<AlertKind>(["disk", "update", ...(boardOk ? (["offline"] as const) : []), ...(earnings ? (["earnings"] as const) : [])]);
  const { notices, writes } = transitions({ settings, findings: f, states, evaluated, fleetNodes: new Set(nodes.map((n) => n.node)), now });
  if (notices.length) {
    const msg = composeNotice(notices, d.site);
    const channels: string[] = [];
    let ok = false;
    if (settings.email && settings.email_verified) {
      try {
        await d.channels.email(settings.email, msg.subject, msg.lines);
        channels.push("email");
        ok = true;
      } catch (e) {
        console.error("fleet alerts: email failed", e);
      }
    }
    if (settings.sms && d.channels.sms) {
      const sent = await d.channels.sms(userId, msg.sms, notices.some((n) => n.event === "alert") ? "warning" : "info").catch(() => false);
      if (sent) {
        channels.push("sms");
        ok = true;
      }
    }
    // Nothing went out: keep the incidents unsent so the next tick tries again.
    if (!ok) {
      const unsent = new Set(notices.filter((n) => n.event === "alert").map((n) => stateKey(n.node, n.kind)));
      for (const w of writes) if (unsent.has(stateKey(w.node, w.kind))) w.notified = false;
    }
    for (const n of notices) await d.store.log(userId, n, channels, ok).catch(() => {});
  }
  if (writes.length) await d.store.writeStates(userId, writes);
  return notices;
}

/** Evaluate every account that has somewhere to send alerts. */
export async function evaluateAll(d: EvalDeps): Promise<number> {
  let sent = 0;
  for (const r of await d.store.recipients()) {
    try {
      sent += (await evaluateAccount(r.user_id, r.settings, d)).length;
    } catch (e) {
      console.error("fleet alerts: account pass failed", r.user_id, e);
    }
  }
  return sent;
}
