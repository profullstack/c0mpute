# The house lane: confidential inference on Profullstack-operated nodes

Most c0mpute chat runs on the open network: any node serving the model can win the job.
Jobs are end-to-end encrypted in transit (DIP-0032), but **the node that wins decrypts and
runs the prompt**, so its operator can read it. That is fine for most prompts and wrong for
some, such as a patient's health records.

The house lane is for those. A request in the house lane runs **only** on a node Profullstack
operates, is **paid only**, and is **not kept**.

## Using it

```http
POST https://c0mpute.com/v1/chat/completions
Authorization: Bearer c0k_...            # an API key, or an OAuth key with scope "inference"
x-c0mpute-lane: house                     # or body: "c0mpute": { "lane": "house" }
content-type: application/json

{ "messages": [...], "max_tokens": 800, "temperature": 0.2, "stream": true }
```

- `model` is ignored: the lane serves the house model (`C0MPUTE_HOUSE_LANE_MODEL`).
- Plain chat only: `tools` are refused (400); chat plugins, resumable runs and the model
  router never apply.
- Responses carry `x-c0mpute-lane: house`. JSON answers add
  `x_c0mpute: { lane, model, operator, charged_usd }`; a stream ends with one
  `{"object":"c0mpute.house","x_c0mpute":{...}}` chunk before `[DONE]` and sends
  `: keepalive` comment lines while the node is busy.
- **402 `balance_required`** (with `top_up`): no compute balance. The lane never falls back
  to the free tier. Add funds at <https://c0mpute.com/checkout>.
- **503 `house_lane_unavailable`**: the house node (or the lane) is down. Nothing else runs
  the request; try again later.

Apps get a key for a person through OAuth (`/.well-known/oauth-authorization-server`,
dynamic public clients, PKCE S256) with **scope `inference`**: chat completions billed to
that person's balance, plus `GET /api/v1/me` and `GET /api/v1/balance`. Nothing else on the
account. Revoke with `POST /api/oauth/revoke` (`token=...`, RFC 7009).

## Who can see what

| Party | Sees the prompt and answer? | Keeps them? |
|---|---|---|
| The app that sent it (e.g. tleehealth) | yes (it wrote it) | its own policy |
| c0mpute.com | in memory while relaying | **no**: not logged, no run, no history |
| Infernet (infernetprotocol.com) | in memory while relaying; encrypted at rest in its job queue while the job runs | **no**: the prompt, answer and token stream are deleted when the request ends; a per-minute sweep deletes anything a crash left. Kept: model, node, timestamps, token counts, the charge |
| The house node (Profullstack-operated, vienna) | yes: it runs the model | **no**: Ollama does not log request bodies |
| Any other node or operator on the network | **never** | n/a |

Database backups taken while a request is in flight can hold its encrypted prompt until the
backups rotate.

## How it is wired

- Infernet **house-lane reservation** (`reservations.house_lane = true`), pinned to one
  Profullstack provider whose `trust_tier` is `private` (the open market never routes to
  it). c0mpute holds the reservation key in `C0MPUTE_HOUSE_LANE_KEY` (`ifr_res_`) and calls
  Infernet with it plus its trusted client key (`X-Infernet-Client-Key`) and the payer
  (`X-Infernet-Billed-Subject`, the person's CoinPay `sub`).
- Infernet refuses a house-lane request that is not from a trusted client (403) or whose
  payer has no balance (402), pins the job to the reservation's node with no fallback
  (503 when it cannot), bills the job to the payer at the ordinary per-token price, and
  purges the content (`jobs.no_retain`).
- c0mpute refuses any 200 that does not carry `x-infernet-lane: house` (fail closed).
- The house node runs its own Ollama (`ollama-house.service`, `127.0.0.1:11435`, `Nice=-5`)
  and its own Infernet provider daemon, separate from the node's open-market work.

Env on c0mpute.com: `C0MPUTE_HOUSE_LANE_KEY`, `C0MPUTE_HOUSE_LANE_MODEL`, and the existing
`INFERNET_CLIENT_KEY` / `INFERNET_URL`. Without them every house-lane request is 503.
