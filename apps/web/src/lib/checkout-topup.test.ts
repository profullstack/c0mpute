import { describe, expect, test } from "bun:test";
import type { Order } from "@profullstack/checkout";
import { TOPUP_CUSTOM, TOPUP_OPTIONS, checkoutPath, topUpAccount, topUpOnInfernet } from "./checkout-topup";

/**
 * /checkout opens a compute top-up on Infernet as the trusted client, for the
 * signed-in person's CoinPay sub. lib/chat/server.ts cannot be imported here
 * (it pulls in the SQLite Cloud driver), so `infernet` is a recorder that
 * answers from a queue: no network.
 */
type Call = { path: string; method?: string; subject?: string | null; headers: Record<string, string>; body: Record<string, unknown> };

function harness(replies: { status: number; body?: unknown }[]) {
  const calls: Call[] = [];
  const linked: { sub: string; name?: string | null }[] = [];
  const warnings: string[] = [];
  const queue = [...replies];
  const pay = topUpOnInfernet({
    infernet: async (path, init = {}, opts = {}) => {
      calls.push({
        path,
        method: init.method,
        subject: opts.subject,
        headers: { ...((init.headers ?? {}) as Record<string, string>) },
        body: JSON.parse(String(init.body ?? "{}")),
      });
      const next = queue.shift();
      if (!next) throw new Error("unexpected extra call to infernet");
      return new Response(next.body === undefined ? "" : JSON.stringify(next.body), {
        status: next.status,
        headers: { "content-type": "application/json" },
      });
    },
    link: async (info) => {
      linked.push(info);
      return {};
    },
    siteUrl: () => "https://c0mpute.com",
    warn: (m) => warnings.push(m),
  });
  return { pay, calls, linked, warnings };
}

const order = (totalCents: number, account: Record<string, unknown> = {}): Order =>
  ({
    site: "c0mpute.com",
    option: { id: "25", name: "$25", kind: "once", months: 0 },
    audience: "",
    quantity: 1,
    code: "",
    source: "",
    currency: "USD",
    unitCents: totalCents,
    subtotalCents: totalCents,
    discountCents: 0,
    totalCents,
    amount: totalCents / 100,
    account: { id: "coinpay-sub-1", email: "", ...account },
    description: "c0mpute.com: $25",
  }) as Order;

const created = (url = "https://coinpayportal.com/pay/cs_1") => ({
  status: 201,
  body: { data: { topup: { id: "tu_1", status: "pending", amount_usd: 25, checkout_url: url }, checkout_url: url, next: "open checkout_url" } },
});

describe("top-up pay()", () => {
  test("asks Infernet for a top-up for the signed-in person and returns the payment page", async () => {
    const { pay, calls, linked, warnings } = harness([created()]);
    const out = await pay(order(2500));

    expect(out).toEqual({ url: "https://coinpayportal.com/pay/cs_1", id: "tu_1" });
    expect(calls).toHaveLength(1);
    expect(calls[0].path).toBe("/api/v1/billing/topup");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].subject).toBe("coinpay-sub-1");
    expect(calls[0].headers["content-type"]).toBe("application/json");
    expect(calls[0].body).toEqual({
      amount_usd: 25,
      success_url: "https://c0mpute.com/checkout/thanks",
      cancel_url: "https://c0mpute.com/checkout",
    });
    expect(linked).toEqual([]);
    expect(warnings).toEqual([]);
  });

  test("sends a typed amount in dollars, cents kept", async () => {
    const { pay, calls } = harness([created()]);
    await pay(order(1234));
    expect(calls[0].body.amount_usd).toBe(12.34);
  });

  test("reads checkout_url from the topup when the top level has none", async () => {
    const { pay } = harness([{ status: 201, body: { data: { topup: { id: "tu_2", checkout_url: "https://coinpayportal.com/pay/cs_2" } } } }]);
    expect(await pay(order(500))).toEqual({ url: "https://coinpayportal.com/pay/cs_2", id: "tu_2" });
  });

  test("subject_not_linked: links the account, then retries once", async () => {
    const { pay, calls, linked } = harness([{ status: 404, body: { error: "subject_not_linked" } }, created()]);
    const out = await pay(order(2500, { name: "anthony" }));

    expect(out.url).toBe("https://coinpayportal.com/pay/cs_1");
    expect(linked).toEqual([{ sub: "coinpay-sub-1", name: "anthony" }]);
    expect(calls).toHaveLength(2);
    expect(calls[1].subject).toBe("coinpay-sub-1");
    expect(calls[1].body).toEqual(calls[0].body);
  });

  test("subject_not_linked in an { error: { code } } body is understood too", async () => {
    const { pay, linked } = harness([{ status: 404, body: { error: { code: "subject_not_linked", message: "no account" } } }, created()]);
    await pay(order(2500));
    expect(linked).toHaveLength(1);
  });

  test("still unlinked after the retry: throws, and does not try a third time", async () => {
    const { pay, calls, linked } = harness([
      { status: 404, body: { error: "subject_not_linked" } },
      { status: 404, body: { error: "subject_not_linked" } },
    ]);
    await expect(pay(order(2500))).rejects.toThrow("HTTP 404");
    expect(calls).toHaveLength(2);
    expect(linked).toHaveLength(1);
  });

  test("a 404 that is not subject_not_linked does not link or retry", async () => {
    const { pay, calls, linked } = harness([{ status: 404, body: { error: "not_found" } }]);
    await expect(pay(order(2500))).rejects.toThrow("HTTP 404");
    expect(calls).toHaveLength(1);
    expect(linked).toEqual([]);
  });

  for (const [status, error] of [
    [400, "amount_below_minimum"],
    [400, "return_url_not_allowed"],
    [429, "rate_limited"],
    [502, "coinpay_unavailable"],
    [200, ""],
  ] as const) {
    test(`HTTP ${status} throws and logs the status${error ? " and error" : ""}, never the subject`, async () => {
      const { pay, calls, warnings } = harness([{ status, body: error ? { error } : created().body }]);
      await expect(pay(order(2500))).rejects.toThrow(`HTTP ${status}`);
      expect(calls).toHaveLength(1);
      expect(warnings).toHaveLength(1);
      expect(warnings[0]).toContain(`HTTP ${status}`);
      if (error) expect(warnings[0]).toContain(error);
      expect(warnings[0]).not.toContain("coinpay-sub-1");
    });
  }

  test("a 201 with no usable checkout_url throws", async () => {
    const none = harness([{ status: 201, body: { data: { topup: { id: "tu_3" } } } }]);
    await expect(none.pay(order(2500))).rejects.toThrow("no checkout_url");
    const notHttps = harness([created("javascript:alert(1)")]);
    await expect(notHttps.pay(order(2500))).rejects.toThrow("no checkout_url");
  });

  test("an error body that is not JSON still throws on the status", async () => {
    const { pay, warnings } = harness([{ status: 502 }]);
    await expect(pay(order(2500))).rejects.toThrow("HTTP 502");
    expect(warnings[0]).toBe("infernet top-up failed: HTTP 502");
  });
});

describe("top-up account()", () => {
  test("is the CoinPay sub", () => {
    expect(topUpAccount({ coinpay_sub: "coinpay-sub-1", name: "anthony" })).toEqual({ id: "coinpay-sub-1", email: "", name: "anthony" });
  });

  test("is null without a coinpay_sub (passkey or password-only account) or when signed out", () => {
    expect(topUpAccount({ coinpay_sub: null, name: "anthony" })).toBeNull();
    expect(topUpAccount({ coinpay_sub: "" })).toBeNull();
    expect(topUpAccount(null)).toBeNull();
    expect(topUpAccount(undefined)).toBeNull();
  });
});

describe("what is for sale", () => {
  test("$5, $10, $25 (preselected) and $100, with Infernet's limits on a typed amount", () => {
    expect(TOPUP_OPTIONS.map((o) => [o.id, "price" in o ? o.price : null])).toEqual([
      ["5", 5],
      ["10", 10],
      ["25", 25],
      ["100", 100],
    ]);
    expect(TOPUP_OPTIONS.filter((o) => o.popular).map((o) => o.id)).toEqual(["25"]);
    expect(TOPUP_CUSTOM).toEqual({ min: 5, max: 10000, default: 50 });
  });

  test("chat estimates are the price over $0.005 a chat", () => {
    for (const o of TOPUP_OPTIONS) {
      const chats = Math.round(Number("price" in o ? o.price : 0) / 0.005);
      expect(o.gets).toBe(`about ${chats.toLocaleString("en-US")} chats`);
    }
  });
});

describe("checkoutPath (the sign-in return link)", () => {
  test("keeps only the package's link parameters", () => {
    expect(checkoutPath({})).toBe("/checkout");
    expect(checkoutPath({ option: "custom", amount: "40", source: "chat", evil: "//x", next: "/a", error: "bad_origin" })).toBe(
      "/checkout?option=custom&amount=40&source=chat",
    );
    expect(checkoutPath({ plan: ["10", "25"] })).toBe("/checkout?plan=10");
  });

  test("stays short enough for safeNext (512 chars)", () => {
    const long = "x".repeat(5000);
    expect(checkoutPath({ option: long, plan: long, amount: long, code: long, source: long }).length).toBeLessThan(512);
  });
});
