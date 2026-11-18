// Copies every credited job out of the status aggregators into SQLite Cloud,
// so node earnings outlive the aggregators' in-memory 24h window. Started
// from instrumentation.ts; no "server-only" import so it loads there.
import { aggregatorUrls } from "../aggregators";
import { cloudSql } from "../chat/server";
import { cleanJob, createEarningsStore, type AggregatorJob, type EarningsStore } from "./store";

let _store: EarningsStore | null = null;
export const earningsStore = () => (_store ??= createEarningsStore(cloudSql));

// ── recorder ─────────────────────────────────────────────────────────────────

/** Pages read per aggregator per pass; each page is up to 2,000 jobs. */
const MAX_PAGES = 20;

/**
 * Copy every job one aggregator credited since our cursor for it. Returns the
 * number of jobs read; an aggregator without `GET /jobs` (older than this
 * site) is skipped quietly.
 */
export async function recordFrom(base: string, store: EarningsStore = earningsStore(), fetcher: typeof fetch = fetch): Promise<number> {
  let since = await store.cursor(base);
  let read = 0;
  for (let page = 0; page < MAX_PAGES; page++) {
    let body: { jobs?: unknown; next_since?: unknown; more?: unknown };
    try {
      const r = await fetcher(`${base}/jobs?since=${since}`, { signal: AbortSignal.timeout(5_000), cache: "no-store" });
      if (!r.ok) return read;
      body = (await r.json()) as typeof body;
    } catch {
      return read;
    }
    const jobs = (Array.isArray(body.jobs) ? body.jobs : []).map(cleanJob).filter((j): j is AggregatorJob => j !== null);
    if (jobs.length) await store.recordJobs(jobs);
    read += jobs.length;
    const next = typeof body.next_since === "number" && Number.isFinite(body.next_since) ? body.next_since : since;
    // The aggregator restarted with a slower clock, or lost its state: start over
    // from its window. The job id keeps every copy to one row.
    if (next < since && jobs.length === 0) {
      await store.setCursor(base, 0);
      return read;
    }
    if (next > since) {
      since = next;
      await store.setCursor(base, since);
    }
    if (body.more !== true) break;
  }
  return read;
}

/** One pass over every aggregator. */
export async function recordAll(): Promise<number> {
  const counts = await Promise.all(aggregatorUrls().map((u) => recordFrom(u).catch(() => 0)));
  return counts.reduce((a, b) => a + b, 0);
}

let started = false;
/** Every 2 minutes, for the life of the server (instrumentation.ts starts it). */
export function startRecorder(everyMs = 120_000) {
  if (started || aggregatorUrls().length === 0) return;
  started = true;
  let running = false;
  const tick = async () => {
    if (running) return;
    running = true;
    try {
      await recordAll();
    } catch (e) {
      console.error("node earnings recorder: pass failed", e);
    } finally {
      running = false;
    }
  };
  // Give the co-located aggregator a moment to come up first.
  const first = setTimeout(tick, 15_000);
  (first as { unref?: () => void }).unref?.();
  const t = setInterval(tick, everyMs);
  (t as { unref?: () => void }).unref?.();
}
