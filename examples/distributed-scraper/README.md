# Distributed scraper

Fetch a list of pages across the c0mpute network. Each URL is one
`crawl.fetch` job with its own auction, so the fetches spread over the crawl
workers that bid and come from their IPs, not yours. Every worker runs the
crawl role by default.

What a worker does with a fetch: a static `GET` (no JavaScript rendering),
robots.txt honoured (`ignore_robots: true` only for a site you own), public
addresses only (re-checked on every redirect, at most 5), at most 1 MiB per
page, 15 s default and 30 s maximum per fetch. A node takes up to 8 fetches
at once.

## One page, no bridge

```sh
c0mpute crawl fetch https://example.com/ --max-price 0.01
# {"job_id":"…","worker":"12D3KooW…","price":0.0,"output_hash":"…",
#  "result":{"v":1,"ok":true,"duration_ms":412,
#            "output":{"url":"https://example.com/","status":200,"headers":{…},"body":"<!doctype html>…"}}}
```

## Many pages, through the bridge

Start a buyer bridge once (it holds the libp2p connection and runs up to 32
jobs at a time):

```sh
export C0MPUTE_JOBS_TOKEN=$(openssl rand -hex 24)
c0mpute jobs serve --listen 127.0.0.1:8788 --max-price 0.01
```

The raw request, one per URL:

```sh
curl -s http://127.0.0.1:8788/v1/jobs \
  -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \
  -d '{
    "workload": "crawl.fetch",
    "spec": {"v": 1, "url": "https://example.com/", "timeout_ms": 20000},
    "max_price": 0.005,
    "timeout_ms": 60000
  }'
```

Fan a list out from the shell, 8 in flight:

```sh
grep -v '^#' urls.txt | xargs -P 8 -I{} curl -s http://127.0.0.1:8788/v1/jobs \
  -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" \
  -d '{"workload":"crawl.fetch","spec":{"v":1,"url":"{}"}}' \
  | jq -c '{url: .result.output.url, worker, status: .result.output.status, ok: .result.ok}'
```

Or with the script here (Node 18+, no dependencies), one JSON line per URL:

```sh
node scrape.mjs urls.txt > pages.jsonl
CONCURRENCY=16 MAX_PRICE=0.005 node scrape.mjs https://example.com https://example.org
# {"url":"https://example.com/","ok":true,"worker":"12D3KooWA…","price":0,"status":200,"bytes":1256,"title":"Example Domain","duration_ms":388}
# {"url":"https://example.org/","ok":true,"worker":"12D3KooWQ…","price":0,"status":200,"bytes":1256,"title":"Example Domain","duration_ms":402}
```

## Request fields

| field | meaning |
| --- | --- |
| `workload` | `"crawl.fetch"` |
| `spec.v` | `1` |
| `spec.url` | `http` or `https` URL to fetch |
| `spec.headers` | optional request headers (no hop-by-hop or credential headers) |
| `spec.timeout_ms` | optional fetch timeout on the worker (default 15 s, max 30 s) |
| `spec.ignore_robots` | skip robots.txt; only for a site you own |
| `max_price` | USD per fetch; may only lower the bridge's `--max-price` |
| `allow_peers` | optional list of worker peer ids allowed to bid |
| `timeout_ms` | whole job, auction included (1 s to 300 s, default 60 s) |

A failed fetch still answers 200 with `"result": {"ok": false, "error": "…"}`
(robots.txt refusal, private address, timeout, page over 1 MiB). Bridge
errors are non-200 with `{"error": "<code>", "detail": "…"}`: 400
`bad_request`, 401 `unauthorized`, 503 `no_bids` (no crawl worker bid under
your price), 502 `worker_failed`, 504 `timeout`. Retry 503/502/504; a retry
is a fresh auction and usually lands on another worker.

The worker sees the URL and headers you send. Keep secrets out of them, or
pin workers you trust with `allow_peers` or `C0MPUTE_JOBS_ALLOW_PEERS`.
