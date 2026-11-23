import { describe, expect, test } from "bun:test";
import { HOUSE_HANDLES, REF_COOKIE, paidRefCode, refFromCookieHeader, refFromUrl } from "./referral-cookie";
import { buildLeaderboard } from "./leaderboard/build";

/**
 * Spend referrals: the first referral code a visitor arrives with is kept so
 * the CoinPay sign-in can hand it to Infernet when the account is created.
 * Infernet decides the rest (real code, new account, not self, not house).
 */
describe("referral first touch", () => {
  test("paid codes only: the house handle and board handles carry no payout", () => {
    expect(paidRefCode("Abcd2345")).toBe("abcd2345");
    expect(paidRefCode("profullstack")).toBeNull();
    expect(HOUSE_HANDLES).toContain("profullstack");
    expect(paidRefCode("my-handle")).toBeNull();
    expect(paidRefCode("abc")).toBeNull();
    expect(paidRefCode("a".repeat(17))).toBeNull();
    expect(paidRefCode(null)).toBeNull();
  });
  test("?ref= on any page and the /r/<code> landing both count", () => {
    expect(refFromUrl(new URL("https://c0mpute.com/advisor/finance?ref=abcd2345"))).toBe("abcd2345");
    expect(refFromUrl(new URL("https://c0mpute.com/r/abcd2345"))).toBe("abcd2345");
    expect(refFromUrl(new URL("https://c0mpute.com/r/abcd2345/"))).toBe("abcd2345");
    expect(refFromUrl(new URL("https://c0mpute.com/advisor/finance?ref=profullstack"))).toBeNull();
    expect(refFromUrl(new URL("https://c0mpute.com/r/profullstack"))).toBeNull();
    expect(refFromUrl(new URL("https://c0mpute.com/chat"))).toBeNull();
    expect(refFromUrl(new URL("https://c0mpute.com/r/abcd2345/extra"))).toBeNull();
  });
  test("reads the stored code back from a Cookie header", () => {
    expect(refFromCookieHeader(`a=1; ${REF_COOKIE}=abcd2345; b=2`)).toBe("abcd2345");
    expect(refFromCookieHeader(`${REF_COOKIE}=profullstack`)).toBeNull();
    expect(refFromCookieHeader("a=1")).toBeNull();
    expect(refFromCookieHeader(null)).toBeNull();
  });
});

describe("affiliates board with spend referrals", () => {
  test("people referred ride along, commissions are whatever Infernet summed (node + spend)", () => {
    const secret = "s".repeat(40);
    const tok = (n: string) => n.padEnd(64, "0");
    const w = {
      providers: [],
      spenders: [],
      referrers: [
        { who: tok("a"), commissions_usd: 0.006, nodes: 1, people: 2 },
        { who: tok("b"), commissions_usd: 0.002, nodes: 0, people: 1 },
        { who: tok("c"), commissions_usd: 0.001, nodes: 1 },
      ],
      models: [],
      totals: { spenders: 0, spent_usd: 0, billed_jobs: 0, earning_providers: 0, served_jobs: 0, output_tokens: 0 },
    };
    const per = <T,>(v: T) => ({ "24h": v, "7d": v, "30d": v, all: v });
    const lb = buildLeaderboard({
      generatedAt: "2026-11-23T12:00:00.000Z",
      secret,
      workersOnline: 0,
      nodes: [],
      referralHandles: [],
      nodeStats: per([]),
      infernet: per(w),
      usage: per([]),
      paidToOperators: per(0),
      hiddenTokens: new Set(),
      hiddenNodes: new Set(),
      history: { node_jobs_from: null, usage_from: null },
    } as unknown as Parameters<typeof buildLeaderboard>[0]);
    const rows = lb.boards["7d"].affiliates.rows;
    expect(rows.map((r) => r.commissions_usd)).toEqual([0.006, 0.002, 0.001]);
    expect(rows.map((r) => r.people_referred)).toEqual([2, 1, 0]);
    // pseudonyms only, never the token or a sub
    for (const r of rows) expect(r.affiliate).toMatch(/^anon-/);
  });
});
