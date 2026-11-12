---
dip: 0035
title: "Route every chat request: pick models and an execution pattern per task, check the answer, show the receipt"
status: Accepted
authors:
  - riotcoder
created: 2026-11-12
updated: 2026-11-12
discussion:
implementation: apps/web/src/lib/router/, apps/web/src/lib/chat/routed.ts
supersedes:
superseded-by:
---

## Summary

`model: "auto"` (the default when no model is named) now goes through a model
router instead of letting whichever node answers first pick. For each
request it classifies the task, scores every live model for it, chooses an
execution pattern (single, cascade, critique or fuse) by expected quality,
cost and latency for the chosen effort, runs it with deterministic checks and
fallbacks, learns from the outcome, and returns a receipt of every call.
Every fleet app gets it through `/v1/chat/completions` (via
@profullstack/ai-providers) with no change.

## Motivation

Before this, "auto" meant the field was dropped: Infernet chose, and on the
p2p path a worker answered with the alphabetically first model it served. A
greeting and a distributed-systems design question went to the same
arbitrary model, a 0.5B model could answer a tax question while our tax
fine-tune sat idle, and a truncated or looping answer went straight to the
user. GitHub Copilot Fusion (GA 2026-10-28) showed the shape of the fix
(runtime routing with single / cascade / critique patterns and an effort
dial) but leaves weaknesses we can fix.

## Detailed design

### Surface

* `model`: a model id goes straight to it, unchanged. `"auto"`, absent,
  `"auto:fast" | "auto:standard" | "auto:high" | "auto:max"` route.
* `router: { effort?, max_cost_usd?, max_latency_ms? }` refines a request.
* Responses carry `x-c0mpute-route: pattern=...; model=...; cost_usd=...`
  and, in JSON or on the last SSE chunk, the full `x_c0mpute_route` receipt:
  classification, the plan and why, alternatives with their estimates, and
  every call with tokens, cost, time and verdicts.
* `POST /api/v1/route` returns the plan without running it (free, public).
* `/v1/models` lists the `auto` entries first; the /chat picker offers them.
* `C0MPUTE_ROUTER=off` disables routing (the old direct path).

### Pipeline

1. **Classify** (`classify.ts`, no model call): category (chat, code, math,
   reasoning, extract, writing, summarize, translate, legal, tax, business),
   difficulty 0..1, prompt size, JSON wanted, script. Every signal that fired
   is listed in `reasons`.
2. **Profile** (`registry.ts`): the live model ids and Infernet's price table
   become profiles (family, size, coder / specialist / uncensored / tiny,
   context, price, providers busy). Priors by size and family; house
   fine-tunes (not-a-lawyer, tax-guy, saas-strategist) get a large bonus in
   their own category and a penalty elsewhere. Uncensored models are left
   out unless allowed.
3. **Learn** (`stats.ts`): per (model, category) a Beta posterior (prior as 6
   pseudo-observations), per model latency per token and an error EWMA; 3
   errors in a row bench a model for 2 minutes. Persisted in `router_stats`.
4. **Plan** (`planner.ts`): every candidate plan gets expected quality, cost
   and latency. Acceptance is `q^(0.4 + 1.6 * difficulty)`: easy work is easy
   for small models, hard work punishes them. A cascade's cost is draft +
   check + P(escalate) x strong model, so it is only chosen when it beats
   going straight to the strong model. Utility = quality - w_cost x cost -
   w_latency x seconds, weights by effort, under the request's budget and a
   quality floor (no plan below 75-95% of the best single model).
   * fast: single models only.
   * standard: + cascades checked deterministically.
   * high: + cascades with an LLM judge, + critique (2 critics, revise).
   * max: + fuse (3 diverse drafters in parallel, best model synthesizes).
   Critique and fuse only for real work (difficulty >= 0.25 or a work
   category), never for small talk.
5. **Execute** (`engine.ts`): deterministic checks on every draft (empty,
   stub text, cut off, repetition loop, refusal, invalid JSON, unclosed or
   unbalanced code, JS syntax, wrong language); a failing draft escalates
   (standard and up) even on a `single` plan. Errors fall back through up to
   2 alternates. **Hedging**: if the draft has not answered by 1.4x (fast) or
   2x (standard) its expected latency, the same request goes to the next
   model and the first answer wins. Budgets are checked before every
   optional step.
6. **Stream**: at fast/standard a streamed request goes straight through on
   the planned model (first token as fast as a direct request); the stream
   is teed and verified at the end so it still teaches the router. High/max
   run the plan, keep the connection alive with SSE comments, then stream
   the answer.

### Better than Copilot Fusion

| Fusion | This router |
|---|---|
| Classifier, quality bar and judge criteria unpublished | Every signal, estimate and verdict in the receipt; `/api/v1/route` explains a plan for free |
| An LLM judge decides escalation | Deterministic checks first (free, exact); a judge only at high/max |
| No visible expected-cost check on cascades | Cascade chosen only when expected cost beats the strong model alone |
| Hidden drafts, additive billing | Per-call cost in the receipt; hard `max_cost_usd` / `max_latency_ms` |
| Coarse three-level dial | Effort plus explicit cost and latency ceilings |
| Static pool health | Learns per model and per category from real outcomes; benches failing models |
| No tail-latency handling | Hedged requests cut slow-node tails |
| Generalist pool | Routes domain questions to house fine-tunes |

## Security and cost

* Judge, critic and synthesis prompts mark task and answer text as data.
* Each routed call is billed like a normal call to the caller; the receipt
  shows every one. Default (standard) adds a call only when the first answer
  failed its checks.
* Learned stats are aggregate counts per model and category: no prompt text
  is stored.

## Not done yet

* Speculative decoding on worker nodes (qwen2.5:0.5b drafting for the 7B/14B
  Qwen models via llama-server) would speed every call; it is a worker
  runtime change, not a router one.
* Plugin (tool-calling) turns still go direct.
* User feedback (thumbs up/down) as a learning signal.
