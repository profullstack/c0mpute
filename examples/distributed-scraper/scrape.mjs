#!/usr/bin/env node
// Distributed scraper on c0mpute: one `crawl.fetch` job per URL, sent through
// a local `c0mpute jobs serve` bridge. Every request is its own auction, so the
// fetches spread over whichever crawl workers bid, each from its own IP.
//
//   C0MPUTE_JOBS_TOKEN=... node scrape.mjs urls.txt > pages.jsonl
//   C0MPUTE_JOBS_TOKEN=... node scrape.mjs https://example.com https://example.org
//
// Env: C0MPUTE_JOBS_URL (default http://127.0.0.1:8788), C0MPUTE_JOBS_TOKEN
// (required), CONCURRENCY (default 8, the bridge runs at most 32 at once),
// MAX_PRICE (USD per fetch; may only lower the bridge's --max-price).
// Output: one JSON line per URL on stdout, failures included.

import { readFileSync } from "node:fs";

const bridge = process.env.C0MPUTE_JOBS_URL ?? "http://127.0.0.1:8788";
const token = process.env.C0MPUTE_JOBS_TOKEN;
const concurrency = Number(process.env.CONCURRENCY ?? 8);
const maxPrice = process.env.MAX_PRICE ? Number(process.env.MAX_PRICE) : undefined;

if (!token) {
  console.error("set C0MPUTE_JOBS_TOKEN (the token `c0mpute jobs serve` was started with)");
  process.exit(2);
}

const args = process.argv.slice(2);
if (args.length === 0) {
  console.error("usage: node scrape.mjs <urls.txt | url ...>");
  process.exit(2);
}
const urls = args
  .flatMap((a) => (/^https?:\/\//.test(a) ? [a] : readFileSync(a, "utf8").split("\n")))
  .map((l) => l.trim())
  .filter((l) => l && !l.startsWith("#"));

async function fetchOne(url) {
  const res = await fetch(`${bridge}/v1/jobs`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      workload: "crawl.fetch",
      spec: { v: 1, url, timeout_ms: 20_000 },
      ...(maxPrice !== undefined && { max_price: maxPrice }),
      timeout_ms: 60_000,
    }),
  });
  const job = await res.json().catch(() => ({}));
  if (!res.ok) {
    const error = [job.error ?? `HTTP ${res.status}`, job.detail].filter(Boolean).join(": ");
    return { url, ok: false, worker: job.worker, error };
  }
  const r = job.result ?? {};
  if (!r.ok) return { url, ok: false, worker: job.worker, error: r.error };
  const body = typeof r.output.body === "string" ? r.output.body : JSON.stringify(r.output.body);
  return {
    url,
    ok: true,
    worker: job.worker,
    price: job.price,
    status: r.output.status,
    bytes: body.length,
    title: body.match(/<title[^>]*>([^<]*)<\/title>/i)?.[1]?.trim() ?? null,
    duration_ms: r.duration_ms,
  };
}

// A small worker pool: `concurrency` jobs in flight at once.
let next = 0;
await Promise.all(
  Array.from({ length: Math.min(concurrency, urls.length) }, async () => {
    while (next < urls.length) {
      const url = urls[next++];
      const row = await fetchOne(url).catch((e) => ({ url, ok: false, error: String(e) }));
      process.stdout.write(JSON.stringify(row) + "\n");
    }
  }),
);
