# c0mpute Experts

First-party AI experts on [c0mpute.com/experts](https://c0mpute.com/experts), answered by open
models on the c0mpute / Infernet network:

| slug | what it does |
|---|---|
| `security-analyst` | CVE and scanner-finding triage, ThreatCrush and vu1nz results, hardening |
| `sales-coach` | signal-based outreach, cold email, follow-ups, objection handling |
| `automation-engineer` | meshhook workflows, scripts, timers, GitHub Actions and CI debugging |
| `legal-info` | general legal information (not advice), with a persistent disclaimer |
| `tax-info` | general tax information (not advice), with a persistent disclaimer |

No therapy, social-work or medical experts: those categories are refused by the manifest
validator whoever signs them.

## Pricing

- **90 days free**, one trial per CoinPay account (bound to the CoinPay `sub`, never an email),
  covering every expert. The first question starts it; `POST /api/v1/experts/trial` starts it
  explicitly. A second account on the same CoinPay sub gets the same trial.
- Then **$9 per expert** or **$29 for every expert**, per 30 days, paid through CoinPay
  checkout. Paying early adds to the time left.
- Inference itself is billed like any c0mpute chat (the person's compute balance, or the free
  daily allowance). The subscription is the access fee.
- Knobs (env, read at request time): `C0MPUTE_EXPERTS_TRIAL_DAYS`, `C0MPUTE_EXPERTS_PRICE_USD`
  (overrides every manifest's list price), `C0MPUTE_EXPERTS_BUNDLE_PRICE_USD`,
  `C0MPUTE_EXPERTS_PERIOD_DAYS`, `C0MPUTE_EXPERTS_ASKS_PER_HOUR`. Defaults in
  `apps/web/src/lib/experts/config.ts`.

## Surfaces

| surface | how |
|---|---|
| Web | `/experts`, `/experts/<slug>` (ask, trial state, subscribe), `/account` (Experts card) |
| OpenAI API | `POST /v1/chat/completions` with `"model": "expert:<slug>"` (JSON or SSE). Listed in `/v1/models`. |
| REST | `GET /api/v1/experts` (public catalog, signed manifests), `GET /api/v1/experts/<slug>`, `POST /api/v1/experts/<slug>/ask` `{question}` or `{messages}`, `GET /api/v1/experts/me`, `POST /api/v1/experts/trial`, `POST /api/v1/experts/checkout` `{plan: "all" or slug, email}` |
| MCP | `list_experts`, `ask_expert`, `experts_status` on `https://c0mpute.com/api/mcp` |
| CLI | `c0mpute experts [status, trial, subscribe <plan> --email]`, `c0mpute ask <slug> "question"` |

Errors: 401 signed out, 403 `coinpay_required` (with a `connect` link), 402 `trial_ended`
(with `subscribe` and `plans`), 429 rate limit. Answers carry `x-c0mpute-expert: <slug>@<version>`,
`x-c0mpute-disclaimer` (URI-encoded, legal/tax) and `x_c0mpute_expert` in the JSON body.

CoinPay payments arrive at the c0mpute.com business webhook
(`/api/v1/gallery/webhook`), which hands `metadata.kind = "experts_subscription"` events to the
Experts service before the gallery.

## How an expert answers

The manifest is applied to an ordinary chat turn: its `system_prompt` replaces any caller system
prompt, its `model.prefer` list is tried in order within one 280 s budget (fail over on 5xx,
404, an nginx page or an empty answer; a 4xx stops), and `model.max_tokens` caps the answer.
Upstream calls are always non-streaming so the answer can be checked (and a leaked `<think>`
block stripped) before anyone sees it; a `stream: true` caller gets it as one SSE chunk.
Model choice (2026-11-22): `qwen2.5-coder:7b` answered general security and sales questions
in 11-21 s on the live network, `qwen3:4b` returned empty content (its thinking used the budget),
`qwen2.5:3b` and `gemma3:4b` had no live node, and the house fine-tunes
(not-a-lawyer-agent, tax-guy-agent) invented case law, so strong general models with strict
prompts are used instead of them. When the person has connected a chat plugin
whose host matches one of the manifest's `tools.mcp` URLs, the plugin tool loop runs with only
those plugins. Prompts require staying in scope, saying when unsure and never inventing
citations, CVE ids, statutes or numbers.

## Manifest format: `c0mpute.manifest/v1`

The same signed format the open p2p app store will use. An expert is `kind: "expert"`.

```jsonc
{
  "format": "c0mpute.manifest/v1",
  "kind": "expert",
  "id": "profullstack/security-analyst",      // <publisher.id>/<slug>
  "slug": "security-analyst",
  "name": "Security Analyst",
  "version": "1.0.0",                          // semver; bump on every change
  "category": "security",                      // security | sales | automation | legal | tax
  "summary": "...", "description": "...",
  "publisher": { "id": "profullstack", "name": "Profullstack", "did": "did:web:profullstack.com",
                 "key": "ed25519:<base64url raw 32-byte public key>" },
  "model": { "prefer": ["qwen2.5-coder:7b", "qwen3:4b", "nemotron-3-nano:4b"], "temperature": 0.3, "max_tokens": 1400 },
  "system_prompt": "...",
  "skills": ["CVE triage", "..."],
  "tools": { "mcp": [{ "id": "threatcrush", "url": "https://threatcrush.com/api/mcp", "why": "..." }] },
  "example_questions": ["..."],
  "disclaimer": null,                          // required for legal and tax
  "pricing": { "model": "subscription", "monthly_usd": 9, "trial_days": 90 },
  "created_at": "2026-11-22T00:00:00Z",
  "signature": { "alg": "ed25519", "key": "ed25519:<same as publisher.key>", "sig": "<base64url>" }
}
```

**Signature:** ed25519 over the canonical JSON of the manifest without `signature`: object keys
sorted at every depth, no whitespace, `undefined` dropped (`canonicalJson` in
`apps/web/src/lib/experts/manifest.ts`). `signature.key` must equal `publisher.key`.

**Admission** (`admitManifest`) is separate from validity: well-formed, signature verifies,
publisher id AND key on the store's allowlist, and not refused by the DIP-0036 labeler
(ThreatCrush by default; the labeler being down fails open). Today the allowlist is first-party
only (`FIRST_PARTY_PUBLISHERS` in `catalog.ts`). Third-party publishing is switched on with
`C0MPUTE_EXPERTS_PUBLISHERS="id=ed25519:key,..."` (or by extending the list), with no format
change.

## Changing an expert

1. Edit `apps/web/src/lib/experts/manifests/<slug>.json`, bump `version`.
2. Sign: pull `C0MPUTE_EXPERTS_SIGNING_KEY` from the logicsrc vault
   `profullstack / c0mpute-experts / signing` (a signing-only vault no deploy pulls), then
   `C0MPUTE_EXPERTS_SIGNING_KEY=... bun scripts/sign-expert-manifests.ts`.
3. `bun test apps/web/src/lib/experts` (refuses an unsigned or tampered manifest), PR, merge.

See also [DIP 0037](../dips/0037-signed-app-manifests.md).
