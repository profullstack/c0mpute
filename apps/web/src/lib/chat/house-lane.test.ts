import { describe, expect, test } from "bun:test";
import { subjectSignature } from "./billed-subject";
import { houseLaneChat, houseLaneConfig, houseStream, wantsHouseLane, type HouseLaneDeps } from "./house-lane";

/**
 * The house lane (x-c0mpute-lane: house): confidential chat that runs only on
 * a Profullstack-operated node through an Infernet house-lane reservation.
 *
 * What must hold: it is paid only (402 without a balance, never the free
 * tier); it fails closed (503 when the house node, Infernet or this server's
 * wiring is not there, and nothing else is ever called); it sends the
 * reservation key, the trusted client key and the payer, and never a model
 * the caller picked; an answer that did not come through the house lane is
 * refused even with a 200.
 */

const ENV: Record<string, string> = {
  C0MPUTE_HOUSE_LANE_KEY: "ifr_res_testkey",
  INFERNET_CLIENT_KEY: "ifc_trusted",
  C0MPUTE_HOUSE_LANE_MODEL: "qwen3:8b",
  INFERNET_URL: "https://infernet.test",
  C0MPUTE_INFERNET_SUBJECT_SECRET: "h".repeat(48),
};
const TOP_UP = "https://c0mpute.com/checkout";
const req = (stream = false) => ({ messages: [{ role: "user" as const, content: "what was my last A1c?" }], stream, max_tokens: 400 });

function deps(over: Partial<HouseLaneDeps> & { respond?: (url: string, init: RequestInit) => Response | Promise<Response> } = {}) {
  const calls: { url: string; init: RequestInit }[] = [];
  const respond =
    over.respond ??
    (() =>
      new Response(JSON.stringify({ id: "x", object: "chat.completion", model: "qwen3:8b", choices: [{ index: 0, message: { role: "assistant", content: "6.1%" } }], x_infernet: { lane: "house", operator: "Profullstack", model: "qwen3:8b", billed: true, charged_usd: "0.001000" } }), {
        status: 200,
        headers: { "content-type": "application/json", "x-infernet-lane": "house" },
      }));
  const d: HouseLaneDeps = {
    fetch: (async (url: string, init: RequestInit) => {
      calls.push({ url: String(url), init });
      return respond(String(url), init);
    }) as unknown as typeof fetch,
    env: (n) => (over.env ? over.env(n) : ENV[n] ?? ""),
    balance: over.balance ?? (async () => ({ total_spendable_usd: "2.500000" })),
    topUpUrl: TOP_UP,
  };
  return { d, calls };
}

describe("wantsHouseLane", () => {
  const r = (h: Record<string, string> = {}) => new Request("https://c0mpute.com/v1/chat/completions", { method: "POST", headers: h });
  test("header or body opts in; anything else does not", () => {
    expect(wantsHouseLane(r({ "x-c0mpute-lane": "house" }), null)).toBe(true);
    expect(wantsHouseLane(r({ "x-c0mpute-lane": "HOUSE " }), null)).toBe(true);
    expect(wantsHouseLane(r(), { c0mpute: { lane: "house" } })).toBe(true);
    expect(wantsHouseLane(r(), { c0mpute: { lane: "open" } })).toBe(false);
    expect(wantsHouseLane(r({ "x-c0mpute-lane": "open" }), {})).toBe(false);
  });
});

describe("houseLaneConfig", () => {
  test("needs a reservation key, the trusted client key and a model", () => {
    expect(houseLaneConfig((n) => ENV[n] ?? "")).toMatchObject({ reservationKey: "ifr_res_testkey", model: "qwen3:8b", infernetUrl: "https://infernet.test" });
    expect(houseLaneConfig((n) => (n === "C0MPUTE_HOUSE_LANE_KEY" ? "" : ENV[n] ?? ""))).toBeNull();
    expect(houseLaneConfig((n) => (n === "C0MPUTE_HOUSE_LANE_KEY" ? "ik_live_wrong" : ENV[n] ?? ""))).toBeNull();
    expect(houseLaneConfig((n) => (n === "INFERNET_CLIENT_KEY" ? "" : ENV[n] ?? ""))).toBeNull();
  });
});

describe("houseLaneChat", () => {
  test("sends the reservation key, trusted key and payer to Infernet, with the house model", async () => {
    const { d, calls } = deps();
    const res = await houseLaneChat({ subject: "cp-sub-1", req: { ...req(), model: "eriktonon/qwen3.8-uncensored:9b" }, deps: d });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-c0mpute-lane")).toBe("house");
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("https://infernet.test/v1/chat/completions");
    const h = calls[0].init.headers as Record<string, string>;
    expect(h.authorization).toBe("Bearer ifr_res_testkey");
    expect(h["x-infernet-client-key"]).toBe("ifc_trusted");
    expect(h["x-infernet-billed-subject"]).toBe("cp-sub-1");
    // Signed: subject|ts|request id with the shared secret.
    expect(h["x-infernet-subject-ts"]).toMatch(/^\d+$/);
    expect(h["x-infernet-request-id"]).toMatch(/^[A-Za-z0-9_-]{8,128}$/);
    expect(h["x-infernet-subject-sig"]).toBe(
      subjectSignature(ENV.C0MPUTE_INFERNET_SUBJECT_SECRET, "cp-sub-1", h["x-infernet-subject-ts"], h["x-infernet-request-id"]),
    );
    const sent = JSON.parse(String(calls[0].init.body));
    expect(sent.model).toBe("qwen3:8b");
    expect(sent.p2p_only).toBeUndefined();
    const body = await res.json();
    expect(body.choices[0].message.content).toBe("6.1%");
    expect(body.x_c0mpute).toEqual({ lane: "house", model: "qwen3:8b", operator: "Profullstack", charged_usd: "0.001000" });
    expect(body.x_infernet).toBeUndefined();
  });

  test("402 without a billable identity or with an empty balance, before calling anything", async () => {
    for (const [subject, balance] of [
      [null, async () => ({ total_spendable_usd: "5.000000" })],
      ["cp-sub-1", async () => ({ total_spendable_usd: "0.000000" })],
    ] as const) {
      const { d, calls } = deps({ balance });
      const res = await houseLaneChat({ subject, req: req(), deps: d });
      expect(res.status).toBe(402);
      const b = await res.json();
      expect(b.error.reason).toBe("balance_required");
      expect(b.error.top_up).toBe(TOP_UP);
      expect(calls).toHaveLength(0);
    }
  });

  test("Infernet's 402 (no account, free tier, degraded) is a 402, not a free answer", async () => {
    const { d } = deps({
      balance: async () => null,
      respond: () => new Response(JSON.stringify({ error: { code: "balance_required", reason: "balance_required", spendable_usd: "0.000000" } }), { status: 402, headers: { "content-type": "application/json" } }),
    });
    const res = await houseLaneChat({ subject: "cp-sub-1", req: req(), deps: d });
    expect(res.status).toBe(402);
    expect((await res.json()).error.reason).toBe("balance_required");
  });

  test("fails closed: node down, Infernet down, HTML page, refused wiring, network error -> 503, one call only", async () => {
    const cases: (() => Response | Promise<Response>)[] = [
      () => new Response(JSON.stringify({ error: { code: "pinned_node_unavailable" } }), { status: 503, headers: { "content-type": "application/json" } }),
      () => new Response("<html>502</html>", { status: 502, headers: { "content-type": "text/html" } }),
      () => new Response(JSON.stringify({ error: { code: "house_lane_client_required" } }), { status: 403, headers: { "content-type": "application/json" } }),
      () => {
        throw new TypeError("fetch failed");
      },
    ];
    for (const respond of cases) {
      const { d, calls } = deps({ respond });
      const res = await houseLaneChat({ subject: "cp-sub-1", req: req(), deps: d });
      expect(res.status).toBe(503);
      expect((await res.json()).error.reason).toBe("house_lane_unavailable");
      expect(calls).toHaveLength(1);
    }
  });

  test("not configured -> 503 and nothing is called", async () => {
    const { d, calls } = deps({ env: () => "" });
    const res = await houseLaneChat({ subject: "cp-sub-1", req: req(), deps: d });
    expect(res.status).toBe(503);
    expect(calls).toHaveLength(0);
  });

  test("a 200 that did not come through the house lane is refused", async () => {
    const { d } = deps({
      respond: () =>
        new Response(JSON.stringify({ choices: [{ message: { content: "from somewhere else" } }] }), { status: 200, headers: { "content-type": "application/json" } }),
    });
    const res = await houseLaneChat({ subject: "cp-sub-1", req: req(), deps: d });
    expect(res.status).toBe(503);
    expect(await res.text()).not.toContain("somewhere else");
  });

  test("tools are refused on the house lane", async () => {
    const { d, calls } = deps();
    const res = await houseLaneChat({ subject: "cp-sub-1", req: { ...req(), tooling: { tools: [], choice: "auto" } as never }, deps: d });
    expect(res.status).toBe(400);
    expect(calls).toHaveLength(0);
  });

  test("streams through, renaming the receipt chunk", async () => {
    const sse =
      'data: {"choices":[{"delta":{"role":"assistant","content":""}}]}\n\n' +
      'data: {"choices":[{"delta":{"content":"6.1%"}}]}\n\n' +
      'data: {"id":"x","object":"infernet.house","x_infernet":{"lane":"house","operator":"Profullstack","model":"qwen3:8b","billed":true,"charged_usd":"0.000900"}}\n\n' +
      "data: [DONE]\n\n";
    const { d } = deps({ respond: () => new Response(sse, { status: 200, headers: { "content-type": "text/event-stream", "x-infernet-lane": "house" } }) });
    const res = await houseLaneChat({ subject: "cp-sub-1", req: req(true), deps: d });
    expect(res.headers.get("x-c0mpute-lane")).toBe("house");
    const text = await res.text();
    expect(text).toContain('"content":"6.1%"');
    expect(text).toContain('"object":"c0mpute.house"');
    expect(text).toContain('"charged_usd":"0.000900"');
    expect(text).not.toContain("infernet.house");
    expect(text.trim().endsWith("data: [DONE]")).toBe(true);
  });
});

describe("houseStream keepalives", () => {
  test("sends a comment line while the node is quiet, only between events", async () => {
    let push!: (s: string) => void;
    let end!: () => void;
    const src = new ReadableStream<Uint8Array>({
      start(c) {
        const e = new TextEncoder();
        push = (s) => c.enqueue(e.encode(s));
        end = () => c.close();
      },
    });
    const out = houseStream(src, "qwen3:8b", 20);
    const reader = out.getReader();
    const chunks: string[] = [];
    const reading = (async () => {
      const d = new TextDecoder();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(d.decode(value));
      }
    })();
    push('data: {"choices":[{"delta":{"content":"a');
    await new Promise((r) => setTimeout(r, 70));
    push('"}}]}\n\ndata: [DONE]\n\n');
    end();
    await reading;
    const all = chunks.join("");
    expect(all).toContain(": keepalive");
    // The split event arrives whole: no keepalive inside it.
    expect(all).toContain('data: {"choices":[{"delta":{"content":"a"}}]}\n\n');
  });
});
