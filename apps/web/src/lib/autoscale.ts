// Node autoscale (docs/autoscale.md): an operator's own machine runs
// `c0mpute autoscale`, which stops a rented GPU node when the network has no
// work for it and starts it again when jobs for its models are waiting. This
// site only (1) serves the network signal it needs, merged from every status
// aggregator, and (2) keeps the state the CLI reports, for /dashboard/nodes.
//
// The operator's cloud provider API key never reaches this site: the CLI holds
// it on the operator's machine and sends only the node's state.

import type { Sql } from "./chat/store";
import { nodeKey } from "./earnings/store";

// ── the signal ───────────────────────────────────────────────────────────────

export interface SignalNode {
  id: string;
  peer_id: string | null;
  online: boolean;
  last_seen_s: number | null;
  in_flight: number;
  last_accept_ms: number | null;
  last_receipt_ms: number | null;
  /** Models the node's capability ad says it serves (union across aggregators). */
  serves: string[];
}

export interface SignalModel {
  offers_window: number;
  offers_2m: number;
  waiting: number;
  unserved_window: number;
  last_offer_ms: number | null;
}

/** What one aggregator's `GET /autoscale` answers (and what we answer, merged). */
export interface Signal {
  now_ms: number;
  window_s: number;
  node: SignalNode;
  models: Record<string, SignalModel>;
}

export const MAX_MODELS = 16;
export const MAX_MODEL_LEN = 200;
export const DEFAULT_WINDOW_S = 1200;
export const MAX_WINDOW_S = 86_400;

export interface SignalQuery {
  node: string;
  peer_id: string | null;
  models: string[];
  window_s: number;
}

/** Validate `?node=&models=&window_s=`; a string is the error. */
export function parseSignalQuery(params: URLSearchParams): SignalQuery | string {
  const k = nodeKey(params.get("node"));
  if (!k) return "node: a peer id or the 8-character id on /nodes";
  const raw = params.get("models") ?? "";
  const models = [...new Set(raw.split(",").map((m) => m.trim()).filter(Boolean))];
  if (models.length > MAX_MODELS) return `models: at most ${MAX_MODELS}`;
  if (models.some((m) => m.length > MAX_MODEL_LEN || /[\s<>"]/.test(m))) return `models: names up to ${MAX_MODEL_LEN} characters, no spaces`;
  const w = params.get("window_s");
  let window_s = DEFAULT_WINDOW_S;
  if (w !== null && w !== "") {
    const n = Number(w);
    if (!Number.isInteger(n) || n < 60 || n > MAX_WINDOW_S) return `window_s: whole seconds from 60 to ${MAX_WINDOW_S}`;
    window_s = n;
  }
  return { node: k.node, peer_id: k.peer_id, models, window_s };
}

const fin = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null);
const maxOf = (a: number | null, b: number | null) => (a === null ? b : b === null ? a : Math.max(a, b));
const minOf = (a: number | null, b: number | null) => (a === null ? b : b === null ? a : Math.min(a, b));

/**
 * Merge the answers of several aggregators (each sees the network from its own
 * seat): online if any saw it, the freshest sighting, and the most work and
 * demand any of them saw. Junk fields count as unknown. Null when none.
 */
export function mergeSignals(answers: unknown[], q: SignalQuery, now = Date.now()): Signal | null {
  const ok = answers.filter((a): a is Record<string, unknown> => !!a && typeof a === "object" && !!(a as Record<string, unknown>).node);
  if (ok.length === 0) return null;
  const node: SignalNode = { id: q.node, peer_id: q.peer_id, online: false, last_seen_s: null, in_flight: 0, last_accept_ms: null, last_receipt_ms: null, serves: [] };
  const serves = new Set<string>();
  const models: Record<string, SignalModel> = {};
  for (const m of q.models) models[m] = { offers_window: 0, offers_2m: 0, waiting: 0, unserved_window: 0, last_offer_ms: null };
  for (const a of ok) {
    const n = a.node as Record<string, unknown>;
    if (n.online === true) node.online = true;
    if (!node.peer_id && typeof n.peer_id === "string" && nodeKey(n.peer_id)?.node === q.node) node.peer_id = n.peer_id;
    node.last_seen_s = minOf(node.last_seen_s, fin(n.last_seen_s));
    node.in_flight = Math.max(node.in_flight, Math.floor(fin(n.in_flight) ?? 0));
    node.last_accept_ms = maxOf(node.last_accept_ms, fin(n.last_accept_ms));
    node.last_receipt_ms = maxOf(node.last_receipt_ms, fin(n.last_receipt_ms));
    if (Array.isArray(n.serves)) {
      for (const m of n.serves) if (typeof m === "string" && m.length > 0 && m.length <= MAX_MODEL_LEN && serves.size < 64) serves.add(m);
    }
    const ms = (a.models && typeof a.models === "object" ? a.models : {}) as Record<string, unknown>;
    for (const name of q.models) {
      const m = ms[name] as Record<string, unknown> | undefined;
      if (!m || typeof m !== "object") continue;
      const t = models[name];
      t.offers_window = Math.max(t.offers_window, Math.floor(fin(m.offers_window) ?? 0));
      t.offers_2m = Math.max(t.offers_2m, Math.floor(fin(m.offers_2m) ?? 0));
      t.waiting = Math.max(t.waiting, Math.floor(fin(m.waiting) ?? 0));
      t.unserved_window = Math.max(t.unserved_window, Math.floor(fin(m.unserved_window) ?? 0));
      t.last_offer_ms = maxOf(t.last_offer_ms, fin(m.last_offer_ms));
    }
  }
  node.serves = [...serves].sort();
  return { now_ms: now, window_s: q.window_s, node, models };
}

/** Ask every aggregator (4 s each) and merge; null when none answered. */
export async function fetchSignal(q: SignalQuery, urls: string[], fetcher: typeof fetch = fetch): Promise<Signal | null> {
  const qs = new URLSearchParams({ node: q.peer_id ?? q.node, window_s: String(q.window_s) });
  if (q.models.length) qs.set("models", q.models.join(","));
  const answers = await Promise.all(
    urls.map(async (base) => {
      try {
        const r = await fetcher(`${base}/autoscale?${qs}`, { signal: AbortSignal.timeout(4_000), cache: "no-store" });
        return r.ok ? await r.json() : null;
      } catch {
        return null;
      }
    }),
  );
  return mergeSignals(answers, q);
}

// ── the state the CLI reports ────────────────────────────────────────────────

export const STATES = ["running", "stopped", "starting", "stopping", "replacing", "error"] as const;
export type AutoscaleState = (typeof STATES)[number];

export interface AutoscaleReport {
  node: string;
  peer_id: string | null;
  provider: string;
  instance: string;
  state: AutoscaleState;
  reason: string;
  at_ms: number;
  saved_usd: number;
  running_usd_per_hour: number | null;
  stopped_usd_per_hour: number | null;
  idle_minutes: number | null;
  models: string[];
}

/** Validate a report body; a string is the error. */
export function parseReport(body: unknown, now = Date.now()): AutoscaleReport | string {
  if (!body || typeof body !== "object") return "body: a JSON object";
  const b = body as Record<string, unknown>;
  const k = nodeKey(b.node);
  if (!k) return "node: a peer id or the 8-character id on /nodes";
  const provider = typeof b.provider === "string" ? b.provider.trim().toLowerCase() : "";
  if (!/^[a-z0-9._-]{1,32}$/.test(provider)) return "provider: vast, salad, thunder, ... (up to 32 characters)";
  const instance = typeof b.instance === "string" ? b.instance.trim() : typeof b.instance === "number" ? String(b.instance) : "";
  if (!instance || instance.length > 64 || /[\s<>"]/.test(instance)) return "instance: the provider's instance id (up to 64 characters)";
  if (typeof b.state !== "string" || !(STATES as readonly string[]).includes(b.state)) return `state: one of ${STATES.join(", ")}`;
  const reason = typeof b.reason === "string" ? b.reason.trim().slice(0, 300) : "";
  let at = fin(b.at_ms);
  // Clocks drift; never in the future, never older than a day.
  if (at === null || at > now + 60_000 || at < now - 86_400_000) at = now;
  const saved = fin(b.saved_usd);
  if (b.saved_usd !== undefined && saved === null) return "saved_usd: a number >= 0";
  const rate = (v: unknown, name: string): number | null | string => {
    if (v === undefined || v === null) return null;
    const n = fin(v);
    return n === null || n > 1_000 ? `${name}: dollars an hour, 0 to 1000` : n;
  };
  const run = rate(b.running_usd_per_hour, "running_usd_per_hour");
  if (typeof run === "string") return run;
  const stop = rate(b.stopped_usd_per_hour, "stopped_usd_per_hour");
  if (typeof stop === "string") return stop;
  let idle: number | null = null;
  if (b.idle_minutes !== undefined && b.idle_minutes !== null) {
    idle = fin(b.idle_minutes);
    if (idle === null || idle > 10_080) return "idle_minutes: 0 to 10080";
  }
  let models: string[] = [];
  if (b.models !== undefined) {
    if (!Array.isArray(b.models) || b.models.length > MAX_MODELS || b.models.some((m) => typeof m !== "string" || !m || m.length > MAX_MODEL_LEN)) {
      return `models: up to ${MAX_MODELS} model names`;
    }
    models = [...new Set(b.models as string[])];
  }
  return {
    node: k.node,
    peer_id: k.peer_id,
    provider,
    instance,
    state: b.state as AutoscaleState,
    reason,
    at_ms: Math.floor(at),
    saved_usd: Math.min(saved ?? 0, 1_000_000),
    running_usd_per_hour: run,
    stopped_usd_per_hour: stop,
    idle_minutes: idle,
    models,
  };
}

export const AUTOSCALE_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS autoscale_nodes (
     user_id              TEXT NOT NULL,
     node                 TEXT NOT NULL,
     provider             TEXT NOT NULL,
     instance             TEXT NOT NULL,
     state                TEXT NOT NULL,
     reason               TEXT NOT NULL DEFAULT '',
     last_transition_ms   INTEGER NOT NULL,
     reported_ms          INTEGER NOT NULL,
     saved_usd            REAL NOT NULL DEFAULT 0,
     running_usd_per_hour REAL,
     stopped_usd_per_hour REAL,
     idle_minutes         REAL,
     models               TEXT NOT NULL DEFAULT '[]',
     PRIMARY KEY (user_id, node)
   )`,
  `CREATE TABLE IF NOT EXISTS autoscale_events (
     id      INTEGER PRIMARY KEY AUTOINCREMENT,
     user_id TEXT NOT NULL,
     node    TEXT NOT NULL,
     state   TEXT NOT NULL,
     reason  TEXT NOT NULL DEFAULT '',
     at_ms   INTEGER NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_autoscale_events_node ON autoscale_events(user_id, node, id)`,
];

/** Events kept per node. */
export const MAX_EVENTS = 200;
/** Events the dashboard shows per node. */
export const SHOWN_EVENTS = 20;

export interface AutoscaleNodeOut {
  node: string;
  provider: string;
  instance: string;
  state: AutoscaleState;
  reason: string;
  last_transition_ms: number;
  reported_ms: number;
  saved_usd: number;
  running_usd_per_hour: number | null;
  stopped_usd_per_hour: number | null;
  idle_minutes: number | null;
  models: string[];
  events: { state: AutoscaleState; reason: string; at_ms: number }[];
}

export function createAutoscaleStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of AUTOSCALE_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  return {
    init,

    /** Keep the latest state; log an event when the state or reason changed. */
    async report(userId: string, r: AutoscaleReport, now = Date.now()): Promise<{ changed: boolean }> {
      await init();
      const prev = (await sql.all(`SELECT state, reason, last_transition_ms FROM autoscale_nodes WHERE user_id = ? AND node = ?`, [userId, r.node]))[0];
      const stateChanged = !prev || prev.state !== r.state;
      const changed = stateChanged || prev?.reason !== r.reason;
      const transition = stateChanged ? r.at_ms : Number(prev?.last_transition_ms ?? r.at_ms);
      await sql.run(
        `INSERT OR REPLACE INTO autoscale_nodes (user_id, node, provider, instance, state, reason, last_transition_ms, reported_ms, saved_usd, running_usd_per_hour, stopped_usd_per_hour, idle_minutes, models)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        [
          userId, r.node, r.provider, r.instance, r.state, r.reason, transition, now, r.saved_usd,
          r.running_usd_per_hour, r.stopped_usd_per_hour, r.idle_minutes, JSON.stringify(r.models),
        ],
      );
      if (changed) {
        await sql.run(`INSERT INTO autoscale_events (user_id, node, state, reason, at_ms) VALUES (?, ?, ?, ?, ?)`, [userId, r.node, r.state, r.reason, r.at_ms]);
        await sql.run(
          `DELETE FROM autoscale_events WHERE user_id = ? AND node = ? AND id NOT IN
             (SELECT id FROM autoscale_events WHERE user_id = ? AND node = ? ORDER BY id DESC LIMIT ?)`,
          [userId, r.node, userId, r.node, MAX_EVENTS],
        );
      }
      return { changed };
    },

    async nodes(userId: string): Promise<AutoscaleNodeOut[]> {
      await init();
      const rows = await sql.all(`SELECT * FROM autoscale_nodes WHERE user_id = ? ORDER BY node`, [userId]);
      const out: AutoscaleNodeOut[] = [];
      for (const r of rows) {
        const events = await sql.all(
          `SELECT state, reason, at_ms FROM autoscale_events WHERE user_id = ? AND node = ? ORDER BY id DESC LIMIT ?`,
          [userId, r.node, SHOWN_EVENTS],
        );
        let models: string[] = [];
        try {
          const m = JSON.parse(String(r.models ?? "[]"));
          if (Array.isArray(m)) models = m.filter((x) => typeof x === "string");
        } catch {}
        const num = (v: unknown) => (v == null ? null : Number(v));
        out.push({
          node: String(r.node),
          provider: String(r.provider),
          instance: String(r.instance),
          state: String(r.state) as AutoscaleState,
          reason: String(r.reason ?? ""),
          last_transition_ms: Number(r.last_transition_ms),
          reported_ms: Number(r.reported_ms),
          saved_usd: Number(r.saved_usd) || 0,
          running_usd_per_hour: num(r.running_usd_per_hour),
          stopped_usd_per_hour: num(r.stopped_usd_per_hour),
          idle_minutes: num(r.idle_minutes),
          models,
          events: events.map((e) => ({ state: String(e.state) as AutoscaleState, reason: String(e.reason ?? ""), at_ms: Number(e.at_ms) })),
        });
      }
      return out;
    },
  };
}

export type AutoscaleStore = ReturnType<typeof createAutoscaleStore>;
