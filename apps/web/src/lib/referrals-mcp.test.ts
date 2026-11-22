import { describe, expect, test } from "bun:test";
import { handleRpc } from "./chat/mcp";
import { REFERRAL_TOOLS, referralTool, type Upstream } from "./referrals-mcp";

/**
 * Node referrals on c0mpute.com are a thin client of Infernet (where c0mpute
 * jobs settle and the commission ledger lives). These pin the MCP surface:
 * the tools exist, referral_link creates (POST) and referral_stats reads
 * (GET), and an Infernet error comes back as a tool error, not a crash.
 */
function fakeUpstream(status: number, body: unknown) {
  const seen: { path: string; method: string }[] = [];
  const up: Upstream = async (path, init = {}) => {
    seen.push({ path, method: init.method ?? "GET" });
    return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
  };
  return { up, seen };
}

describe("referral MCP tools", () => {
  test("are listed by tools/list", async () => {
    const { up } = fakeUpstream(200, {});
    const res = (await handleRpc({ jsonrpc: "2.0", id: 1, method: "tools/list" }, { user: { id: "u", did: "did:x", name: null }, upstream: up })) as { result: { tools: { name: string }[] } };
    const names = res.result.tools.map((t) => t.name);
    expect(names).toContain("referral_link");
    expect(names).toContain("referral_stats");
    expect(REFERRAL_TOOLS.length).toBe(2);
  });

  test("referral_link POSTs, referral_stats GETs, both to /api/v1/referrals", async () => {
    const { up, seen } = fakeUpstream(200, { data: { has_code: true, code: "abcd2345" } });
    const link = await referralTool("referral_link", up);
    const stats = await referralTool("referral_stats", up);
    expect(seen).toEqual([
      { path: "/api/v1/referrals", method: "POST" },
      { path: "/api/v1/referrals", method: "GET" },
    ]);
    expect(JSON.parse(link.text).code).toBe("abcd2345");
    expect(stats.isError).toBeUndefined();
  });

  test("an upstream error is a tool error", async () => {
    const { up } = fakeUpstream(404, { error: "no Infernet account is linked to that CoinPay account yet" });
    const r = await referralTool("referral_stats", up);
    expect(r.isError).toBe(true);
    expect(r.text).toContain("linked");
  });

  test("tools/call dispatches through the person-bound upstream", async () => {
    const { up, seen } = fakeUpstream(200, { data: { has_code: false } });
    const res = (await handleRpc(
      { jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "referral_stats", arguments: {} } },
      { user: { id: "u", did: "did:x", name: null }, upstream: up },
    )) as { result: { content: { text: string }[] } };
    expect(JSON.parse(res.result.content[0].text).has_code).toBe(false);
    expect(seen[0].path).toBe("/api/v1/referrals");
  });
});
