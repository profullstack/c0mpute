// Reads live comparison rates from r4ck.dev once a day and keeps them: in
// memory for this process, and in SQLite Cloud so a restart (every deploy)
// does not start from nothing. A page view never waits on r4ck: it gets the
// last snapshot (or null, and the page falls back to the hand-checked table)
// while a refresh runs in the background.
// No "server-only" import, so its tests load it (like ./earnings/recorder).
import { cloudSql } from "./chat/server";
import type { Sql } from "./chat/store";
import { CPU_RATE_REFERENCES } from "./cpu-estimates";
import { GPU_RATE_REFERENCES } from "./gpu-estimates";
import {
  R4CK_SITE,
  cpuSearchParams,
  gpuRatesFrom,
  isStale,
  roundRate,
  type LiveRate,
  type LiveRates,
  type R4ckOffer,
} from "./r4ck-rates";

/** Refresh once a day. */
export const RATES_MAX_AGE_MS = 24 * 3_600_000;
/** Anonymous r4ck pages hold 50 rows; 1,100-odd hourly GPU offers is ~22 pages. */
const PAGE = 50;
const MAX_GPU_PAGES = 40;
/** Space requests out: r4ck's edge answers bursts with 429. */
const GAP_MS = 1_200;

const SCHEMA = `CREATE TABLE IF NOT EXISTS estimate_rates (
  id         TEXT PRIMARY KEY,
  body       TEXT NOT NULL,
  fetched_at INTEGER NOT NULL
)`;
const ROW_ID = "r4ck";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

interface SearchPage {
  total?: number;
  servers?: R4ckOffer[];
  next?: string | null;
}

async function r4ck(path: string, fetcher: typeof fetch): Promise<SearchPage> {
  for (let attempt = 0; ; attempt++) {
    const r = await fetcher(`${R4CK_SITE}${path}`, {
      headers: { accept: "application/json", "user-agent": "c0mpute.com/estimates" },
      signal: AbortSignal.timeout(20_000),
      cache: "no-store",
    });
    if (r.ok) return (await r.json()) as SearchPage;
    if ((r.status === 429 || r.status === 402 || r.status >= 500) && attempt < 2) {
      await sleep(5_000 * (attempt + 1));
      continue;
    }
    throw new Error(`r4ck ${path} answered ${r.status}`);
  }
}

/** Read every hourly GPU offer and the CPU medians from r4ck. Throws when r4ck is down. */
export async function fetchLiveRates(fetcher: typeof fetch = fetch, gapMs = GAP_MS): Promise<LiveRates> {
  const offers: R4ckOffer[] = [];
  for (let page = 0; page < MAX_GPU_PAGES; page++) {
    const q = new URLSearchParams({ kind: "gpu", interval: "hour", limit: String(PAGE), offset: String(page * PAGE) });
    const body = await r4ck(`/api/v1/search?${q}`, fetcher);
    const rows = Array.isArray(body.servers) ? body.servers : [];
    offers.push(...rows);
    if (rows.length < PAGE || !body.next || offers.length >= Number(body.total ?? 0)) break;
    await sleep(gapMs);
  }
  if (offers.length === 0) throw new Error("r4ck listed no hourly GPU offers");
  const gpu = gpuRatesFrom(offers, GPU_RATE_REFERENCES);

  // CPU: the median plan by price, read as the middle row of the sorted list.
  const cpu: Record<string, LiveRate> = {};
  for (const ref of CPU_RATE_REFERENCES) {
    try {
      await sleep(gapMs);
      const base = cpuSearchParams(ref.vcpu, ref.ramGb);
      base.set("limit", "1");
      const first = await r4ck(`/api/v1/search?${base}`, fetcher);
      const total = Number(first.total ?? 0);
      const low = first.servers?.[0]?.price?.hourly_usd;
      if (!total || low == null) continue;
      await sleep(gapMs);
      base.set("offset", String(Math.floor(total / 2)));
      const mid = (await r4ck(`/api/v1/search?${base}`, fetcher)).servers?.[0]?.price?.hourly_usd;
      if (mid == null || !(Number(mid) > 0)) continue;
      cpu[ref.model] = { hourlyUsd: roundRate(Number(mid)), lowUsd: roundRate(Number(low)), offers: total, providers: null };
    } catch {
      // One spec failing leaves that row on its fallback.
    }
  }
  return { fetchedAt: new Date().toISOString(), gpu, cpu, gpuOffersRead: offers.length };
}

export function createRatesCache(sql: Sql, fetcher: () => Promise<LiveRates>) {
  let memory: LiveRates | null = null;
  let loaded = false;
  let refreshing: Promise<void> | null = null;
  let lastFailure = 0;

  const load = async () => {
    if (loaded) return;
    loaded = true;
    try {
      await sql.run(SCHEMA);
      const row = (await sql.all(`SELECT body FROM estimate_rates WHERE id = ?`, [ROW_ID]))[0];
      if (row?.body) memory = JSON.parse(String(row.body)) as LiveRates;
    } catch (e) {
      console.error("estimates: could not read cached r4ck rates", e);
    }
  };

  const refresh = () =>
    (refreshing ??= (async () => {
      try {
        const next = await fetcher();
        memory = next;
        await sql.run(`INSERT OR REPLACE INTO estimate_rates (id, body, fetched_at) VALUES (?, ?, ?)`, [
          ROW_ID,
          JSON.stringify(next),
          Date.parse(next.fetchedAt),
        ]);
      } catch (e) {
        lastFailure = Date.now();
        console.error("estimates: r4ck refresh failed", e);
      } finally {
        refreshing = null;
      }
    })());

  return {
    /** The last snapshot (possibly stale, possibly null); refreshes in the background when due. */
    async get(now = Date.now()): Promise<LiveRates | null> {
      await load();
      // After a failure, wait an hour before asking r4ck again.
      if (isStale(memory, RATES_MAX_AGE_MS, now) && now - lastFailure > 3_600_000) void refresh();
      return memory;
    },
    refresh,
  };
}

let _cache: ReturnType<typeof createRatesCache> | null = null;
export function liveRates(): Promise<LiveRates | null> {
  _cache ??= createRatesCache(cloudSql, () => fetchLiveRates());
  return _cache.get().catch(() => null);
}
