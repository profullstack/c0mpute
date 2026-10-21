/**
 * The status aggregators this site reads (DIP-0014). Anyone can run one, and
 * each sees the network from its own seat, so the site asks every configured
 * aggregator at once and uses the answer that sees the most workers. One
 * aggregator down (or restarting) then changes nothing on /status or /nodes.
 *
 * STATUS_AGGREGATOR_URLS is a comma list; STATUS_AGGREGATOR_URL (one URL) is
 * still read for older deploys.
 */
export function aggregatorUrls(): string[] {
  const raw = [process.env.STATUS_AGGREGATOR_URLS, process.env.STATUS_AGGREGATOR_URL]
    .filter(Boolean)
    .join(",");
  const urls = raw
    .split(",")
    .map((u) => u.trim().replace(/\/+$/, ""))
    .filter((u) => /^https?:\/\//.test(u));
  return [...new Set(urls)];
}

/**
 * GET `path` from every aggregator in parallel (2 s each) and return the JSON
 * body with the highest `workers_online`, or null when none answered.
 */
export async function fetchFromAggregators<T extends { workers_online?: number; network?: { workers_online?: number } }>(
  path: string,
  opts: { revalidate?: number } = {},
  urls: string[] = aggregatorUrls(),
): Promise<T | null> {
  if (urls.length === 0) return null;
  const answers: (T | null)[] = await Promise.all(
    urls.map(async (base): Promise<T | null> => {
      try {
        const r = await fetch(`${base}${path}`, {
          signal: AbortSignal.timeout(2_000),
          ...(opts.revalidate
            ? { next: { revalidate: opts.revalidate } }
            : { cache: "no-store" as const }),
        });
        return r.ok ? ((await r.json()) as T) : null;
      } catch {
        return null;
      }
    }),
  );
  const seen = (a: T) => a.workers_online ?? a.network?.workers_online ?? 0;
  return answers
    .filter((a): a is T => a !== null)
    .reduce<T | null>((best, a) => (best === null || seen(a) > seen(best) ? a : best), null);
}
