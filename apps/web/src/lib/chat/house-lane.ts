// The house lane: confidential chat on nodes Profullstack operates.
//
// A request opts in with the header `x-c0mpute-lane: house` (or body
// `c0mpute: { lane: "house" }`). It then runs ONLY through an Infernet
// house-lane reservation pinned to one Profullstack node, using a reservation
// key (ifr_res_) this server holds; the open network, the c0mpute auction,
// Infernet's free tier and the hosted fallback are never tried. It is paid
// only: billed to the caller's compute balance at the ordinary per-token
// price, and refused (402) without one. If the house node is down the answer
// is 503 and nothing else runs it.
//
// Who can read a house-lane prompt: the app that sent it, this server and
// Infernet's while they relay it (in memory; neither logs it), and the house
// node. Infernet deletes the prompt, answer and token stream when the request
// ends (docs/house-lane.md). Nothing here is stored: no run, no history.
//
// Pure (fetch, env and balance are passed in) so it is testable without the
// SQLite Cloud connection lib/chat/server.ts opens on import.

import type { ChatRequest } from "./completions";
import { billedSubjectHeaders } from "./billed-subject";

export const HOUSE_LANE = "house";
const KEEPALIVE_MS = 15_000;
const TIMEOUT_MS = 290_000;

export type HouseLaneConfig = { infernetUrl: string; reservationKey: string; clientKey: string; model: string };

/** Did the caller ask for the house lane (header or body)? */
export function wantsHouseLane(req: Request, raw: Record<string, unknown> | null): boolean {
  if ((req.headers.get("x-c0mpute-lane") ?? "").trim().toLowerCase() === HOUSE_LANE) return true;
  const c = raw?.c0mpute as Record<string, unknown> | undefined;
  return !!c && typeof c === "object" && c.lane === HOUSE_LANE;
}

/** The lane's wiring from env, or null when it is not configured (then every house request is 503). */
export function houseLaneConfig(env: (name: string) => string): HouseLaneConfig | null {
  const reservationKey = env("C0MPUTE_HOUSE_LANE_KEY").trim();
  const clientKey = env("INFERNET_CLIENT_KEY").trim();
  const model = env("C0MPUTE_HOUSE_LANE_MODEL").trim();
  if (!reservationKey.startsWith("ifr_res_") || !clientKey.startsWith("ifc_") || !model) return null;
  return { infernetUrl: (env("INFERNET_URL") || "https://infernetprotocol.com").replace(/\/$/, ""), reservationKey, clientKey, model };
}

type ErrBody = { error: { message: string; type: string; code: number; reason: string; top_up?: string; [k: string]: unknown } };

function errorResponse(status: number, reason: string, message: string, extra: Record<string, unknown> = {}) {
  const body: ErrBody = { error: { message, type: "c0mpute_error", code: status, reason, lane: HOUSE_LANE, ...extra } };
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", "x-c0mpute-lane": HOUSE_LANE, ...(status === 503 ? { "retry-after": "60" } : {}) },
  });
}

export const balanceRequired = (topUp: string, spendable = "0.000000") =>
  errorResponse(402, "balance_required", `the private house lane is paid only: add funds at ${topUp}`, { top_up: topUp, spendable_usd: spendable });

export const laneUnavailable = (why = "the private house node is not available right now; try again later") =>
  errorResponse(503, "house_lane_unavailable", why);

/** The receipt c0mpute hands back, from Infernet's. */
function receipt(x: Record<string, unknown> | undefined, model: string) {
  return {
    lane: HOUSE_LANE,
    model: typeof x?.model === "string" ? x.model : model,
    operator: typeof x?.operator === "string" ? x.operator : "Profullstack",
    ...(typeof x?.charged_usd === "string" ? { charged_usd: x.charged_usd } : { charge_pending: true }),
  };
}

export type HouseLaneDeps = {
  fetch: typeof fetch;
  env: (name: string) => string;
  /** The person's spendable balance (Infernet billing), or null when it cannot be read. */
  balance: (subject: string) => Promise<{ total_spendable_usd?: string } | null>;
  topUpUrl: string;
};

/**
 * Serve one house-lane chat for a signed-in person. Never falls back: every
 * failure is a 402 (no balance) or 503 (lane down) the caller can show.
 */
export async function houseLaneChat(opts: {
  subject: string | null | undefined;
  req: ChatRequest;
  signal?: AbortSignal;
  deps: HouseLaneDeps;
}): Promise<Response> {
  const { subject, req, deps } = opts;
  if (req.tooling) return errorResponse(400, "unsupported", "the house lane takes plain chat only (no tools)");
  // Billing is by CoinPay identity; an account without one has no balance to spend.
  if (!subject) return balanceRequired(deps.topUpUrl);
  const cfg = houseLaneConfig(deps.env);
  if (!cfg) return laneUnavailable("the private house lane is not configured on this server");

  // Refuse an empty balance here rather than round-trip. An unreadable
  // balance (null) is left to Infernet, which refuses it too.
  const bal = await deps.balance(subject).catch(() => null);
  const spendable = Number(bal?.total_spendable_usd ?? NaN);
  if (bal && Number.isFinite(spendable) && spendable <= 0) return balanceRequired(deps.topUpUrl, bal.total_spendable_usd);

  const timeout = AbortSignal.timeout(TIMEOUT_MS);
  let upstream: Response;
  try {
    upstream = await deps.fetch(`${cfg.infernetUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${cfg.reservationKey}`,
        "x-infernet-client-key": cfg.clientKey,
        // Signed (subject|ts|request id) like every other call we make for a
        // person: Infernet refuses an unsigned subject once
        // INFERNET_REQUIRE_SIGNED_SUBJECTS is on.
        ...billedSubjectHeaders(subject, deps.env("C0MPUTE_INFERNET_SUBJECT_SECRET")),
        "content-type": "application/json",
        accept: req.stream ? "text/event-stream" : "application/json",
      },
      body: JSON.stringify({
        model: cfg.model,
        messages: req.messages,
        stream: req.stream === true,
        ...(req.max_tokens ? { max_tokens: req.max_tokens } : {}),
        ...(req.temperature !== undefined ? { temperature: req.temperature } : {}),
      }),
      signal: opts.signal ? AbortSignal.any([timeout, opts.signal]) : timeout,
    });
  } catch {
    return laneUnavailable();
  }

  const type = upstream.headers.get("content-type") ?? "";
  if (!upstream.ok) {
    const body = type.includes("json") ? ((await upstream.json().catch(() => null)) as { error?: Record<string, unknown> } | null) : null;
    if (!body) await upstream.body?.cancel().catch(() => {});
    const e = body?.error ?? {};
    if (upstream.status === 402) return balanceRequired(deps.topUpUrl, typeof e.spendable_usd === "string" ? e.spendable_usd : undefined);
    if (upstream.status === 429) return errorResponse(429, "rate_limited", "the house lane is busy: too many requests; try again shortly");
    if (upstream.status === 400) return errorResponse(400, "bad_request", typeof e.message === "string" ? e.message : "bad request");
    // 403 = this server's wiring (key, trust), 404/5xx = Infernet or the node: the lane is down either way.
    if (upstream.status === 403) console.error(`house lane refused by Infernet: ${String(e.code ?? upstream.status)}`);
    return laneUnavailable();
  }
  // Fail closed: an answer that did not come through the house lane is not one.
  if ((upstream.headers.get("x-infernet-lane") ?? "").toLowerCase() !== HOUSE_LANE) {
    await upstream.body?.cancel().catch(() => {});
    console.error("house lane: upstream answered without x-infernet-lane: house; refused");
    return laneUnavailable();
  }

  const headers = { "cache-control": "no-store", "x-accel-buffering": "no", "x-c0mpute-lane": HOUSE_LANE };
  if (!req.stream) {
    const j = (await upstream.json().catch(() => null)) as Record<string, unknown> | null;
    if (!j || !Array.isArray(j.choices)) return laneUnavailable();
    const { x_infernet, ...rest } = j as { x_infernet?: Record<string, unknown> };
    return new Response(JSON.stringify({ ...rest, x_c0mpute: receipt(x_infernet, cfg.model) }), {
      status: 200,
      headers: { ...headers, "content-type": "application/json" },
    });
  }
  if (!upstream.body) return laneUnavailable();
  return new Response(houseStream(upstream.body, cfg.model), { status: 200, headers: { ...headers, "content-type": "text/event-stream" } });
}

/**
 * Pass Infernet's SSE through event by event, renaming its receipt chunk
 * ("infernet.house" -> "c0mpute.house" with x_c0mpute) and sending a comment
 * line every 15 s while the node is busy (a long prefill on a CPU node), so
 * no proxy in between closes an idle connection.
 */
export function houseStream(body: ReadableStream<Uint8Array>, model: string, keepaliveMs = KEEPALIVE_MS): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const reader = body.getReader();
  let buf = "";
  let timer: ReturnType<typeof setInterval> | null = null;
  let closed = false;
  const rewrite = (event: string) => {
    const line = event.trim();
    if (!line.startsWith("data:")) return event;
    const data = line.slice(5).trim();
    if (!data.includes('"infernet.house"')) return event;
    try {
      const j = JSON.parse(data) as { x_infernet?: Record<string, unknown>; usage?: unknown };
      return `data: ${JSON.stringify({ object: "c0mpute.house", x_c0mpute: receipt(j.x_infernet, model), ...(j.usage ? { usage: j.usage } : {}) })}\n\n`;
    } catch {
      return event;
    }
  };
  return new ReadableStream<Uint8Array>({
    start(ctl) {
      timer = setInterval(() => {
        if (!closed) ctl.enqueue(enc.encode(": keepalive\n\n"));
      }, keepaliveMs);
      (timer as { unref?: () => void }).unref?.();
    },
    async pull(ctl) {
      try {
        const { done, value } = await reader.read();
        if (done) {
          if (buf.trim()) ctl.enqueue(enc.encode(rewrite(buf)));
          closed = true;
          if (timer) clearInterval(timer);
          ctl.close();
          return;
        }
        buf += dec.decode(value, { stream: true });
        let i: number;
        while ((i = buf.indexOf("\n\n")) >= 0) {
          const event = buf.slice(0, i + 2);
          buf = buf.slice(i + 2);
          ctl.enqueue(enc.encode(rewrite(event)));
        }
      } catch (e) {
        closed = true;
        if (timer) clearInterval(timer);
        ctl.error(e);
      }
    },
    async cancel() {
      closed = true;
      if (timer) clearInterval(timer);
      await reader.cancel().catch(() => {});
    },
  });
}
