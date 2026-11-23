import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { randomBytes } from "node:crypto";
import type { Sql } from "./store";
import { createPluginStore, publicPlugin } from "./plugin-store";
import { applyPolicy, DEFAULT_PLUGINS, describeAction, DOCS_CATALOG, FINANCE_CATALOG, mcpAddCommand, needsConfirmation, POLICIES, policyFor, redactLog } from "./plugin-policy";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

describe("money plugin catalog", () => {
  test("Mercury and CoinPay are on by default; moomoo is listed but off", () => {
    expect(DEFAULT_PLUGINS.map((d) => d.slug).sort()).toEqual(["cheapestwayz", "coinpay", "mercury", "microsoft-learn"]);
    const moomoo = FINANCE_CATALOG.find((e) => e.slug === "moomoo")!;
    expect(moomoo.default).toBe(false);
    expect(moomoo.description).toContain("you decide");
    for (const e of FINANCE_CATALOG) {
      expect(e.url.startsWith("https://")).toBe(true);
      expect(e.description.toLowerCase()).not.toContain("first ever");
    }
  });

  test("policy is chosen by server, not by slug", () => {
    expect(policyFor("https://mcp.mercury.com/mcp")?.kind).toBe("banking");
    expect(policyFor("https://MCP.mercury.com/mcp/")?.kind).toBe("banking");
    expect(policyFor("https://coinpayportal.com/api/mcp")?.kind).toBe("crypto");
    expect(policyFor("https://mcp.moomoo.com/mcp")?.kind).toBe("trading");
    expect(policyFor("https://threatcrush.com/api/mcp")).toBeNull();
    expect(policyFor("https://mcp.mercury.com.evil.example/mcp")).toBeNull();
    expect(policyFor("not a url")).toBeNull();
  });
});

describe("confirmation gate", () => {
  test("only allowlisted reads run alone; writes and unknown tools ask", () => {
    const m = POLICIES.mercury;
    expect(needsConfirmation(m, "listTransactions")).toBe(false);
    expect(needsConfirmation(m, "getAccounts")).toBe(false);
    for (const t of ["requestSendMoney", "requestTransferMoney", "approveReimbursement", "createRecipient", "somethingNew"]) expect(needsConfirmation(m, t)).toBe(true);
    const c = POLICIES.coinpay;
    expect(needsConfirmation(c, "invoice_list")).toBe(false);
    for (const t of ["invoice_create_draft", "time_log", "project_invoice_time", "wallet_send"]) expect(needsConfirmation(c, t)).toBe(true);
    const mm = POLICIES.moomoo;
    expect(needsConfirmation(mm, "quote_stock_quote")).toBe(false);
    expect(needsConfirmation(mm, "account_positions")).toBe(false);
    for (const t of ["trading_order_place", "trading_order_cancel", "sim_trade_input_order", "quote_modify_user_security", "transfer_funds"]) expect(needsConfirmation(mm, t)).toBe(true);
  });

  test("a server that calls a write tool read-only cannot loosen the rule", () => {
    const tools = [
      { name: "listTransactions", annotations: {} },
      { name: "requestSendMoney", annotations: { readOnlyHint: true, destructiveHint: false } },
      { name: "mystery" },
    ];
    const out = applyPolicy(POLICIES.mercury, tools);
    expect(out.find((t) => t.name === "listTransactions")?.annotations).toMatchObject({ readOnlyHint: true, destructiveHint: false });
    expect(out.find((t) => t.name === "requestSendMoney")?.annotations).toMatchObject({ readOnlyHint: false, destructiveHint: true });
    expect(out.find((t) => t.name === "mystery")?.annotations).toMatchObject({ destructiveHint: true });
    // No policy: the server's own annotations stand.
    expect(applyPolicy(null, tools)).toBe(tools);
  });

  test("brokerage: paper mode hides live order tools; live shows them (still asking)", () => {
    const tools = ["quote_stock_quote", "sim_trade_input_order", "trading_order_place", "trading_order_cancel", "unknown_tool"].map((name) => ({ name }));
    const paper = applyPolicy(POLICIES.moomoo, tools, "paper").map((t) => t.name);
    expect(paper).toEqual(["quote_stock_quote", "sim_trade_input_order"]);
    const live = applyPolicy(POLICIES.moomoo, tools, "live");
    expect(live.map((t) => t.name)).toEqual(tools.map((t) => t.name));
    expect(live.find((t) => t.name === "trading_order_place")?.annotations).toMatchObject({ destructiveHint: true });
  });
});

describe("approval card", () => {
  test("a payment shows amount, currency, from, to and memo", () => {
    const s = describeAction(POLICIES.mercury, "requestSendMoney", { accountId: "acct_1", recipientId: "rcp_9", amount: 1250.5, currency: "USD", note: "Invoice 42" })!;
    const get = (l: string) => s.lines.find((x) => x.label === l)?.value;
    expect(s.title).toBe("Request send money");
    expect(get("Amount")).toBe("1250.5");
    expect(get("Currency")).toBe("USD");
    expect(get("From")).toBe("acct_1");
    expect(get("To")).toBe("rcp_9");
    expect(get("Memo")).toBe("Invoice 42");
    expect(s.note).toContain("Mercury");
  });

  test("a discounted CoinPay draft invoice shows subtotal, discount and total, never the discount as the amount", () => {
    const s = describeAction(POLICIES.coinpay, "invoice_create_draft", { lineItems: [{ description: "Consulting", quantity: 2, unit_price: 150 }], discount: { type: "percent", value: 10 } })!;
    const get = (l: string) => s.lines.find((x) => x.label === l)?.value;
    expect(get("Subtotal")).toBe("300 (1 line item)");
    expect(get("Discount")).toBe("10% = -30");
    expect(get("Amount")).toBe("270 total (300 subtotal - 30 discount)");
  });

  test("a CoinPay draft invoice totals its line items", () => {
    const s = describeAction(POLICIES.coinpay, "invoice_create_draft", { clientId: "c1", lineItems: [{ description: "work", quantity: 3, unit_price: 100 }] })!;
    expect(s.lines.find((x) => x.label === "Amount")?.value).toContain("300");
    expect(s.lines.find((x) => x.label === "Currency")?.value).toBe("USD");
    expect(s.lines.find((x) => x.label === "To")?.value).toBe("c1");
  });

  test("an order shows symbol, side, quantity, type, prices, estimated cost and paper vs live", () => {
    const live = describeAction(POLICIES.moomoo, "trading_order_place", { acc_id: "123", code: "US.AAPL", side: "BUY", qty: "10", order_type: "LIMIT", price: "190.5", time_in_force: "DAY" })!;
    const get = (l: string) => live.lines.find((x) => x.label === l)?.value;
    expect(live.title).toBe("Place order");
    expect(get("Symbol")).toBe("US.AAPL");
    expect(get("Side")).toBe("BUY");
    expect(get("Quantity")).toBe("10");
    expect(get("Order type")).toBe("LIMIT");
    expect(get("Limit price")).toBe("190.5");
    expect(get("Estimated cost")).toContain("1,905");
    expect(get("Account")).toContain("LIVE");
    expect(live.account).toBe("live");
    expect(live.note).toContain("can be wrong");
    const paper = describeAction(POLICIES.moomoo, "sim_trade_input_order", { code: "US.TSLA", side: "SELL", qty: "1", order_type: "MARKET" })!;
    expect(paper.account).toBe("paper");
    expect(paper.lines.find((x) => x.label === "Estimated cost")?.value).toContain("market price");
    expect(describeAction(null, "x", {})).toBeNull();
  });
});

describe("default plugins in the store", () => {
  // The defaults that sign in on first use (money plugins and CheapestWayz); the docs default is tested below.
  const defaults = DEFAULT_PLUGINS.filter((d) => d.auth !== "none").map((d) => ({ slug: d.slug, name: d.name, url: d.url }));

  test("Microsoft Learn is seeded on and ready to use, with no sign-in", async () => {
    const ms = DOCS_CATALOG.find((e) => e.slug === "microsoft-learn")!;
    expect(ms.url).toBe("https://learn.microsoft.com/api/mcp");
    expect(policyFor(ms.url)).toBeNull();
    expect(mcpAddCommand(ms)).toBe("claude mcp add --transport http microsoft-learn https://learn.microsoft.com/api/mcp");
    const sql = memSql();
    const s = createPluginStore(sql, { key: randomBytes(32), defaults: DEFAULT_PLUGINS });
    const list = await s.list("u1");
    expect(list.map((p) => p.slug).sort()).toEqual(["cheapestwayz", "coinpay", "mercury", "microsoft-learn"]);
    const p = list.find((x) => x.slug === "microsoft-learn")!;
    expect(p.auth).toBe("none");
    const pub = publicPlugin(p, "https://c0mpute.com");
    expect(pub.status).toBe("on");
    expect(pub.connected).toBe(true);
    expect(pub.auth).toBe("none");
    expect(pub.connect_url).toBeNull();
    expect(pub.icon).toBe(ms.icon);
    // Removed stays removed, in a fresh process too.
    expect(await s.remove("u1", "microsoft-learn")).toBe(true);
    const again = createPluginStore(sql, { key: randomBytes(32), defaults: DEFAULT_PLUGINS });
    expect((await again.list("u1")).some((x) => x.slug === "microsoft-learn")).toBe(false);
  });

  test("every account (existing or new) gets them on and not connected, with no account attached", async () => {
    const s = createPluginStore(memSql(), { key: randomBytes(32), defaults });
    for (const user of ["old-user", "new-user"]) {
      const list = await s.list(user);
      expect(list.map((p) => p.slug).sort()).toEqual(["cheapestwayz", "coinpay", "mercury"]);
      for (const p of list) {
        expect(p.enabled).toBe(true);
        expect(p.token).toBeNull();
        expect(p.tokens).toBeNull();
        const pub = publicPlugin(p, "https://c0mpute.com");
        expect(pub.status).toBe("needs_sign_in");
        expect(pub.connected).toBe(false);
        expect(pub.auth).toBe("oauth");
      }
    }
    expect((await s.list("new-user")).some((p) => p.slug === "moomoo")).toBe(false);
  });

  test("a person can turn one off or remove it, and it stays that way", async () => {
    const sql = memSql();
    const s = createPluginStore(sql, { key: randomBytes(32), defaults });
    await s.list("u1");
    expect((await s.setEnabled("u1", "mercury", false))?.enabled).toBe(false);
    expect(await s.remove("u1", "coinpay")).toBe(true);
    // A fresh process (the seeding cache is per instance) does not bring it back.
    const again = createPluginStore(sql, { key: randomBytes(32), defaults });
    const list = await again.list("u1");
    expect(list.map((p) => p.slug).sort()).toEqual(["cheapestwayz", "mercury"]);
    expect(list.find((p) => p.slug === "mercury")!.enabled).toBe(false);
  });

  test("an account that already has the server keeps its own row", async () => {
    const sql = memSql();
    const plain = createPluginStore(sql, { key: null });
    await plain.add("u1", { slug: "bank", url: "https://mcp.mercury.com/mcp", name: "My Mercury", auth: "none" });
    const seeded = createPluginStore(sql, { key: null, defaults });
    const list = await seeded.list("u1");
    expect(list.map((p) => p.slug).sort()).toEqual(["bank", "cheapestwayz", "coinpay"]);
  });

  test("brokerage starts on paper; removing and re-adding starts on paper again", async () => {
    const s = createPluginStore(memSql(), { key: randomBytes(32) });
    await s.add("u1", { slug: "moomoo", url: "https://mcp.moomoo.com/mcp", name: "moomoo", auth: "none" });
    expect((await s.get("u1", "moomoo"))?.trading_mode).toBe("paper");
    expect(publicPlugin((await s.get("u1", "moomoo"))!, "https://c0mpute.com").trading_mode).toBe("paper");
    expect((await s.setTradingMode("u1", "moomoo", "live"))?.trading_mode).toBe("live");
    await s.remove("u1", "moomoo");
    await s.add("u1", { slug: "moomoo", url: "https://mcp.moomoo.com/mcp", name: "moomoo", auth: "none" });
    expect((await s.get("u1", "moomoo"))?.trading_mode).toBe("paper");
  });
});

describe("logs", () => {
  test("tokens, account numbers and amounts are masked", () => {
    const out = redactLog(new Error('token exchange failed: {"access_token":"abc.def","refresh_token":"r1"} Bearer eyJhbGciOi.eyJzdWIi.sig acct 1234567890123 paid $1,250.00'));
    expect(out).not.toContain("abc.def");
    expect(out).not.toContain("r1\"");
    expect(out).not.toContain("1234567890123");
    expect(out).not.toContain("1,250");
    expect(out).toContain("[redacted]");
  });
});
