import { describe, expect, test } from "bun:test";
import { billingClientAllowed, parseScope } from "../chat/oauth";
import { chargeForApp, validateAppCharge, type InfernetCharge } from "./app-charge";

const user = { id: "u_1", coinpay_sub: "cp-sub-1" };
const input = (over: Record<string, unknown> = {}) => {
  const v = validateAppCharge({ idem_key: "meeting:abc", kind: "board.meeting", cost_usd: 0.84, meta: { meeting: "abc" }, ...over });
  if (!v.ok) throw new Error(v.error);
  return v.value;
};

describe("billing scope", () => {
  test("billing is a scope, and only listed clients may ask for it", () => {
    expect(parseScope("inference billing")).toBe("inference billing");
    expect(billingClientAllowed("c0c_board", "c0c_other, c0c_board")).toBe(true);
    expect(billingClientAllowed("c0c_stranger", "c0c_board")).toBe(false);
    expect(billingClientAllowed("c0c_board", "")).toBe(false);
  });
});

describe("validateAppCharge", () => {
  test("only known kinds, positive capped amounts, small meta", () => {
    expect(validateAppCharge({ idem_key: "k", kind: "live.relay", cost_usd: 1 }).ok).toBe(false);
    expect(validateAppCharge({ idem_key: "k", kind: "board.meeting", cost_usd: "1" }).ok).toBe(false);
    expect(validateAppCharge({ idem_key: "k", kind: "board.meeting", cost_usd: 51 }).ok).toBe(false);
    expect(validateAppCharge({ idem_key: "has space", kind: "board.meeting", cost_usd: 1 }).ok).toBe(false);
    expect(validateAppCharge({ idem_key: "k", kind: "board.meeting", cost_usd: 1, meta: { x: "y".repeat(2000) } }).ok).toBe(false);
    expect(input().costUsd).toBe(0.84);
  });
});

describe("chargeForApp", () => {
  test("bills the key's own person as a house charge, scoped idem key", async () => {
    const seen: Parameters<InfernetCharge>[] = [];
    const infernet: InfernetCharge = async (subject, body) => {
      seen.push([subject, body]);
      return { status: 200, data: { data: { charged: true, cost_usd: 0.84, operator_usd: 0, fee_usd: 0.84, balance_usd: 4.16 } } };
    };
    const out = await chargeForApp({ user, client: "Board", input: input(), infernet, topUpUrl: "https://c0mpute.com/checkout" });
    expect(out).toEqual({ status: 200, body: { charged: true, cost_usd: 0.84, balance_usd: 4.16 } });
    expect(seen[0][0]).toBe("cp-sub-1");
    expect(seen[0][1]).toMatchObject({ idem_key: "app:Board:u_1:meeting:abc", kind: "board.meeting", provider: "house", cost_usd: 0.84 });
  });

  test("a short balance or no CoinPay account is a 402 with the top-up link; nothing is sent for no account", async () => {
    const short: InfernetCharge = async () => ({ status: 200, data: { data: { charged: false, reason: "insufficient_balance", balance_usd: 0.1 } } });
    expect(await chargeForApp({ user, client: "Board", input: input(), infernet: short, topUpUrl: "T" })).toEqual({
      status: 402,
      body: { charged: false, reason: "insufficient_balance", balance_usd: 0.1, top_up_url: "T" },
    });
    let called = false;
    const never: InfernetCharge = async () => ((called = true), { status: 200, data: {} });
    const out = await chargeForApp({ user: { id: "u_2", coinpay_sub: null }, client: "Board", input: input(), infernet: never, topUpUrl: "T" });
    expect(out.status).toBe(402);
    expect(called).toBe(false);
  });

  test("Infernet down is a retryable 503; a reused key is a 409", async () => {
    const down: InfernetCharge = async () => {
      throw new Error("ECONNREFUSED");
    };
    expect((await chargeForApp({ user, client: "Board", input: input(), infernet: down, topUpUrl: "T" })).status).toBe(503);
    const reused: InfernetCharge = async () => ({ status: 409, data: { error: "idem_key was already used for a different charge" } });
    expect((await chargeForApp({ user, client: "Board", input: input(), infernet: reused, topUpUrl: "T" })).status).toBe(409);
  });
});
