// Copies each finished hour of disk dues out of the hosting gateways into
// SQLite Cloud (see ./store). Started from instrumentation.ts; no
// "server-only" import so it loads there.
import { cloudSql } from "../chat/server";
import { cleanDue, createDiskStore, type DiskStore, type GatewayDue } from "./store";

let _store: DiskStore | null = null;
export const diskStore = () => (_store ??= createDiskStore(cloudSql));

const HOUR_S = 3600;
/**
 * How far back a gateway can still price an hour. Receipts are kept 16 per
 * holder at one challenge per 10 minutes by default: about 2.7 hours. Older
 * hours would come back undercounted (fewer challenges, so less owed), so a
 * gateway we could not reach for longer is not asked for them.
 */
export const LOOKBACK_S = 2 * HOUR_S;
/** Wait this long after an hour ends: the last receipts are still on gossip. */
export const SETTLE_LAG_S = 5 * 60;

/**
 * The gateways asked. HOSTING_GATEWAY_URLS is a comma list; default the
 * public one. Every gateway sees the whole map, so one is enough and a
 * second only fills in what the first missed.
 */
export function gatewayUrls(): string[] {
  const raw = process.env.HOSTING_GATEWAY_URLS ?? "https://gateway.c0mpute.com";
  return [
    ...new Set(
      raw
        .split(",")
        .map((u) => u.trim().replace(/\/+$/, ""))
        .filter((u) => /^https?:\/\//.test(u)),
    ),
  ];
}

/** The hours to ask `base` for now, oldest first: `[from, to)` in unix seconds. */
export function hoursToRecord(cursor: number | null, nowS: number): [number, number][] {
  const last = Math.floor((nowS - SETTLE_LAG_S) / HOUR_S) * HOUR_S; // end of the newest finished hour
  const oldest = last - LOOKBACK_S;
  let from = cursor == null || cursor < oldest ? oldest : cursor;
  from = Math.floor(from / HOUR_S) * HOUR_S;
  const out: [number, number][] = [];
  while (from + HOUR_S <= last) {
    out.push([from, from + HOUR_S]);
    from += HOUR_S;
  }
  return out;
}

/**
 * Record every finished hour `base` has not given us yet. Returns the dues
 * filed. A gateway older than `GET /hosting/v1/dues` answers 404 and is
 * skipped quietly.
 */
export async function recordFrom(base: string, store: DiskStore = diskStore(), fetcher: typeof fetch = fetch, nowS = Math.floor(Date.now() / 1000)): Promise<number> {
  let filed = 0;
  for (const [from, to] of hoursToRecord(await store.cursor(base), nowS)) {
    let body: { dues?: unknown };
    try {
      const r = await fetcher(`${base}/hosting/v1/dues?from=${from}&to=${to}`, { signal: AbortSignal.timeout(20_000), cache: "no-store" });
      if (!r.ok) return filed;
      body = (await r.json()) as typeof body;
    } catch {
      return filed;
    }
    const dues = (Array.isArray(body.dues) ? body.dues : []).map(cleanDue).filter((d): d is GatewayDue => d !== null);
    if (dues.length) await store.recordHour(from * 1000, dues);
    await store.setCursor(base, to);
    filed += dues.length;
  }
  return filed;
}

export async function recordAll(): Promise<number> {
  const counts = await Promise.all(gatewayUrls().map((u) => recordFrom(u).catch(() => 0)));
  return counts.reduce((a, b) => a + b, 0);
}

let started = false;
/** Every 10 minutes for the life of the server (instrumentation.ts starts it). */
export function startDiskRecorder(everyMs = 600_000) {
  if (started || gatewayUrls().length === 0) return;
  started = true;
  let running = false;
  const tick = async () => {
    if (running) return;
    running = true;
    try {
      await recordAll();
    } catch (e) {
      console.error("disk dues recorder: pass failed", e);
    } finally {
      running = false;
    }
  };
  const first = setTimeout(tick, 30_000);
  (first as { unref?: () => void }).unref?.();
  const t = setInterval(tick, everyMs);
  (t as { unref?: () => void }).unref?.();
}
