import { describe, expect, test } from "bun:test";
import { billedSubjectHeaders, billingSubject, subjectSignature } from "./billed-subject";
import { chatRunOpener } from "./run-opener";
import type { RunRequest } from "./runs";

/**
 * Every call made for a person names that person to Infernet (2026-11-22).
 * An account without CoinPay used to send nothing, so Infernet metered it by
 * our server's IP and every such person shared one ten-a-day free allowance.
 */

const SECRET = "k".repeat(48);
const NOW = Date.UTC(2026, 10, 22, 12, 0, 0);

describe("billingSubject", () => {
  test("a CoinPay account keeps its sub; any other account is c0mpute:<id>", () => {
    expect(billingSubject({ id: "u_1", coinpay_sub: "cp-sub" })).toBe("cp-sub");
    expect(billingSubject({ id: "u_abc", coinpay_sub: null })).toBe("c0mpute:u_abc");
    expect(billingSubject({ id: "u_abc" })).toBe("c0mpute:u_abc");
    expect(billingSubject(null)).toBeNull();
    expect(billingSubject({ id: null, coinpay_sub: null })).toBeNull();
  });
});

describe("billedSubjectHeaders", () => {
  test("signs subject|ts|request id with the shared secret", () => {
    const h = billedSubjectHeaders("c0mpute:u_abc", SECRET, { now: NOW, requestId: "req_12345678" });
    const ts = String(NOW / 1000);
    expect(h).toEqual({
      "x-infernet-billed-subject": "c0mpute:u_abc",
      "x-infernet-subject-ts": ts,
      "x-infernet-request-id": "req_12345678",
      "x-infernet-subject-sig": subjectSignature(SECRET, "c0mpute:u_abc", ts, "req_12345678"),
    });
    expect(h["x-infernet-subject-sig"]).toMatch(/^[0-9a-f]{64}$/);
  });

  test("a fresh request id per call, in the charset Infernet accepts", () => {
    const a = billedSubjectHeaders("c0mpute:u_abc", SECRET)["x-infernet-request-id"];
    const b = billedSubjectHeaders("c0mpute:u_abc", SECRET)["x-infernet-request-id"];
    expect(a).not.toBe(b);
    expect(a).toMatch(/^[A-Za-z0-9_-]{8,128}$/);
  });

  test("no secret: a minted subject is not sent (Infernet would ignore it); a CoinPay one still is", () => {
    expect(billedSubjectHeaders("c0mpute:u_abc", "")).toEqual({});
    expect(billedSubjectHeaders("c0mpute:u_abc", "short")).toEqual({});
    expect(billedSubjectHeaders("cp-sub", undefined)).toEqual({ "x-infernet-billed-subject": "cp-sub" });
    expect(billedSubjectHeaders(null, SECRET)).toEqual({});
  });
});

describe("the chat run opener", () => {
  const sse = "data: {}\n\ndata: [DONE]\n\n";
  const req = (coinpaySub: string | null): RunRequest => ({
    userId: "u_newsignup",
    coinpaySub,
    withPlugins: false,
    body: { messages: [{ role: "user", content: "hello" }], stream: true },
  });

  test("names an unlinked person as c0mpute:<id>, a linked one by CoinPay sub", async () => {
    const seen: Array<string | null> = [];
    const open = chatRunOpener((subject) => {
      seen.push(subject);
      return (async () => new Response(sse, { headers: { "content-type": "text/event-stream" } })) as never;
    });
    await open(req(null), new AbortController().signal);
    await open(req("cp-sub"), new AbortController().signal);
    expect(seen).toEqual(["c0mpute:u_newsignup", "cp-sub"]);
  });
});
