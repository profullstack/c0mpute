# x402: charge bots for the files your node serves

Every c0mpute gateway serves hosted sites (`/s/<site>/...` and custom domains)
and storage objects to the public. Most of what fetches them in bulk is AI
training crawlers. The `x402` plugin, built into the node and **off by
default**, lets an operator charge them for it, through CoinPay, into the
operator's own wallet.

It is the same protocol every Profullstack site speaks
([`@profullstack/x402-gateway`](https://github.com/profullstack/x402-gateway),
here its Rust port from
[profullstack/x402-ports](https://github.com/profullstack/x402-ports)), so any
x402 client (`x402 pay <url>`, `@profullstack/x402-client`) can buy a pass
without knowing it is talking to a c0mpute node.

## Who pays

| Client | What happens |
| --- | --- |
| Declared AI training crawlers (GPTBot, ClaudeBot, anthropic-ai, CCBot, meta-externalagent, FacebookBot, Bytespider, Applebot-Extended) | HTTP 402 with an x402 v2 offer (JSON, or a sales page for `Accept: text/html`) |
| The same crawler with a valid pass | Served normally |
| People, retrieval agents (ChatGPT-User, Claude-User, OAI-SearchBot, PerplexityBot, Bingbot, ...) | Served normally, never charged |
| Browsers sending no `Sec-Fetch-*` headers (scrapers copying a Chrome UA) | Charged only with `charge_spoofed_browsers = true` |
| The network itself: `/hosting/v1/*` (edges and gateways read chunks here), `/p/*` (ciphertext loader), `/healthz` | Never gated |

## Turn it on

```toml
# ~/.config/c0mpute/config.toml
[x402]
enabled = true
# pay_to = "0x..."             # optional override; by default the wallet comes from CoinPay
# price_cents  = 100           # one day
# max_days     = 30            # most days one payment buys (?days=N)
# gate_storage = false         # also gate /storage/v1 object reads
# exclude_sites = ["53021d6e8d5a0e1aac48"]   # never charge for these sites
# exclude_hosts = ["docs.example.com"]       # or these custom domains
# site_url = "https://gw.example.com"        # default: [gateway] announce
```

The CoinPay key goes in the environment, never in config.toml:

```sh
export C0MPUTE_X402_COINPAY_KEY=cp_live_...   # name set by [x402] api_key_env
```

Use a CoinPay business key scoped to x402 (verify + settle). Get one at
[coinpayportal.com](https://coinpayportal.com). Restart the worker. The log
says `x402: charging training crawlers for hosted files` with `source=coinpay`.
The receiving wallet is your CoinPay business's USDC wallet
(`GET /api/x402/pay-to` with the same key, Base preferred), re-read every hour,
so changing it in CoinPay changes it on every node. Until it is known nothing
is charged. If the
key is missing, it says so and stays off, because a gate that cannot settle
would only turn crawlers away for nothing.

## How a sale goes

1. `GET /s/<site>/movie.mp4` with `User-Agent: GPTBot/1.2` → **402**. The body
   carries `accepts` (USDC on Base, Polygon, Ethereum; `payTo` = your
   address), the price, and `buy: <site_url>/_x402/crawl`.
2. The crawler signs an EIP-3009 transfer and retries with `X-PAYMENT`. The
   gateway asks CoinPay to verify and settle it (on a blocking thread, so no
   other request waits), then answers **200** with a receipt whose `pass` is a
   signed `cp_...` token, valid for the days paid.
3. Later requests send `x-crawl-pass: <pass>` (or `Authorization: Bearer`) and
   are served.

`/_x402/crawl` answers every client (so `curl` can see the offer) on every host
the gateway serves. It is namespaced so it never shadows a hosted site's own
`/crawl`.

## Where the money and the records are

- **Money:** settled by CoinPay to `pay_to`. Your CoinPay dashboard lists every
  payment.
- **Sales log:** `<data_dir>/x402/sales.jsonl` (`~/.local/share/c0mpute/x402/`
  on Linux), one JSON line per sale: payer, days, cents, user agent, expiry.
- **Pass secret:** `<data_dir>/x402/secret` (random, mode 0600). Passes survive
  restarts. Delete the file to void every pass this gateway has sold.

## Limits, on purpose

- A pass is good **at the gateway that sold it**. Each operator sells for their
  own gateway. A crawler that round-robins across gateways buys at each one, or
  sticks to one.
- Site owners do not get a share yet. The operator pays for the bandwidth and
  keeps the sale. An owner-set price in the signed site record, split per
  served byte, is the obvious next step.
- A crawler that lies about its user agent is not caught by the list. The
  answer is price, not detection: a day pass costs less than the residential
  proxies needed to dodge it.
- Private mode serves nothing public, so the gate stays off there.
