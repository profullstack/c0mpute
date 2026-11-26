import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { randomBytes } from "node:crypto";
import { ContextSession, OFFER_AT, contextToolsEnabled, stripLabels, tagOf, tagsOf, type Collapse } from "./context-tools";
import { createPluginStore } from "./plugin-store";
import type { Sql } from "./store";

type Msg = { role: string; content: string };
const sys: Msg = { role: "system", content: "You can use tools." };
const u = (content: string): Msg => ({ role: "user", content });
const a = (content: string): Msg => ({ role: "assistant", content });
const big = (n: number) => "x".repeat(n);

/** A conversation of `pairs` long tool calls + results, then a question. */
function convo(pairs: number, size = 2_000): Msg[] {
  const out: Msg[] = [sys, u("TASK: reconcile my accounts")];
  for (let i = 0; i < pairs; i++) out.push(a(`\`\`\`tool_call\n{"name":"bank__list","arguments":{"page":${i}}}\n\`\`\``), u(`<tool_result name="bank__list" status="ok">\npage ${i} ${big(size)}\n</tool_result>`));
  out.push(u("so what is the total?"));
  return out;
}

describe("view", () => {
  test("below the offer line: no labels, no usage note", () => {
    const s = new ContextSession({ budget: 100_000 });
    const msgs = convo(2);
    const v = s.view(msgs);
    expect(v).toEqual(msgs);
  });

  test("past the offer line: every message labelled with its stable tag, usage note in the system prompt", () => {
    const msgs = convo(10);
    const s = new ContextSession({ budget: Math.ceil(msgs.reduce((n, m) => n + m.content.length, 0) / 4 / (OFFER_AT + 0.05)) });
    const v = s.view(msgs);
    expect(v[0].content).toContain("Context window: about");
    expect(v[0].content).toContain("not before");
    expect(v[1].content).toBe(`⟦m:${tagOf(msgs[1])}⟧ ${msgs[1].content}`);
    expect(v).toHaveLength(msgs.length);
  });

  test("past 85% the note asks to compact now", () => {
    const msgs = convo(10);
    const s = new ContextSession({ budget: 3_000 });
    expect(s.view(msgs)[0].content).toContain("It is past 85%");
  });
});

describe("collapse + recall", () => {
  test("collapses a run into the summary, frees tokens, and recall returns the originals", async () => {
    const msgs = convo(10);
    const saved: Collapse[] = [];
    const s = new ContextSession({ budget: 6_000, persist: async (c) => void saved.push(c) });
    s.view(msgs);
    const from = tagOf(msgs[2]);
    const to = tagOf(msgs[15]);
    const r = await s.call("collapse", { from: `⟦m:${from}⟧`, to, summary: "Pages 0-6 listed 7 accounts, total $1,234." });
    expect(r.isError).toBe(false);
    expect(r.text).toContain("Collapsed 14 messages");
    expect(saved).toEqual([{ from, to, summary: "Pages 0-6 listed 7 accounts, total $1,234." }]);

    const v = s.view(msgs);
    expect(v).toHaveLength(msgs.length - 13);
    const sum = v.find((m) => m.content.includes("[Summary of 14 earlier messages"));
    // Back under the offer line after the collapse, so no labels any more.
    expect(sum?.content.startsWith("[Summary of 14")).toBe(true);
    expect(sum?.content).toContain("total $1,234");

    const back = await s.call("recall", { tag: `${from}..${to}` });
    expect(back.isError).toBe(false);
    expect(back.text).toContain("page 0 ");
    expect(back.text).toContain("recall cut at 12000");
    const one = await s.call("recall", { tag: tagOf(msgs[3]) });
    expect(one.text).toContain("page 0 ");
  });

  test("refuses early compaction, unknown tags, the latest message and summaries that save nothing", async () => {
    const roomy = new ContextSession({ budget: 1_000_000 });
    roomy.view(convo(3));
    expect((await roomy.call("collapse", { from: "aaaaaa", to: "bbbbbb", summary: "x" })).text).toContain("nothing to collapse yet");

    const msgs = convo(10);
    const s = new ContextSession({ budget: 4_000 });
    s.view(msgs);
    expect((await s.call("collapse", { from: "000000", to: tagOf(msgs[4]), summary: "x" })).isError).toBe(true);
    const latest = await s.call("collapse", { from: tagOf(msgs[2]), to: tagOf(msgs[msgs.length - 1]), summary: "x" });
    expect(latest.text).toContain("latest message");
    const fat = await s.call("collapse", { from: tagOf(msgs[2]), to: tagOf(msgs[2]), summary: big(500) });
    expect(fat.text).toContain("not shorter");
    expect((await s.call("collapse", { from: tagOf(msgs[2]), to: tagOf(msgs[3]), summary: "" })).isError).toBe(true);
  });

  test("saved collapses apply on a later turn wherever their messages still are", () => {
    const msgs = convo(6);
    const c: Collapse = { from: tagOf(msgs[2]), to: tagOf(msgs[5]), summary: "pages 0-1: nothing unusual" };
    const later = [...msgs.slice(0, -1), a("The total is $9."), u("and last month?")];
    const s = new ContextSession({ budget: 1_000_000, saved: [c] });
    const v = s.view(later);
    expect(v).toHaveLength(later.length - 3);
    expect(v[2].content).toContain("pages 0-1: nothing unusual");
    // A collapse whose messages are gone does nothing.
    const other = new ContextSession({ budget: 1_000_000, saved: [{ from: "abcdef", to: "fedcba", summary: "?" }] });
    expect(other.view(later)).toEqual(later);
  });

  test("identical messages get different tags, so a run of them collapses whole", async () => {
    // Found live 2026-11-26: ten identical tool results shared one tag and
    // `to` resolved to the first copy, collapsing 2 messages instead of 14.
    const msgs: Msg[] = [sys, u("TASK")];
    for (let i = 0; i < 10; i++) msgs.push(a(`call ${i}`), u(`<tool_result name="x" status="ok">\n${big(3_000)}\n</tool_result>`));
    msgs.push(u("total?"));
    const tags = tagsOf(msgs);
    expect(new Set(tags).size).toBe(tags.length);
    expect(tags[3]).toBe(tagOf(msgs[3]));
    const s = new ContextSession({ budget: 9_000 });
    s.view(msgs);
    const r = await s.call("collapse", { from: tags[2], to: tags[15], summary: "pages 0-6 fine" });
    expect(r.text).toContain("Collapsed 14 messages");
    // Tags hold when the conversation grows.
    expect(tagsOf([...msgs, a("ok"), u("more")]).slice(0, msgs.length)).toEqual(tags);
  });

  test("a wider collapse replaces the ones inside it", async () => {
    const msgs = convo(10);
    const s = new ContextSession({ budget: 5_000 });
    s.view(msgs);
    await s.call("collapse", { from: tagOf(msgs[2]), to: tagOf(msgs[5]), summary: "first two pages" });
    s.view(msgs);
    const r = await s.call("collapse", { from: `${tagOf(msgs[2])}..${tagOf(msgs[5])}`, to: tagOf(msgs[11]), summary: "first five pages" });
    expect(r.isError).toBe(false);
    const v = s.view(msgs);
    expect(v.filter((m) => m.content.includes("[Summary of")).length).toBe(1);
    expect(v.find((m) => m.content.includes("[Summary of"))?.content).toContain("first five pages");
  });
});

describe("helpers", () => {
  test("labels are stripped from answers", () => {
    expect(stripLabels("⟦m:a1b2c3⟧ The total is $5 ⟦s:a1b2c3..d4e5f6⟧ ok")).toBe("The total is $5 ok");
  });
  test("CHAT_CONTEXT_TOOLS switch", () => {
    expect(contextToolsEnabled({})).toBe(true);
    expect(contextToolsEnabled({ CHAT_CONTEXT_TOOLS: "off" })).toBe(false);
    expect(contextToolsEnabled({ CHAT_CONTEXT_TOOLS: "0" })).toBe(false);
  });
});

describe("store", () => {
  function memSql(): Sql {
    const db = new Database(":memory:");
    return {
      all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
      run: async (q, p = []) => {
        db.query(q).run(...(p as never[]));
      },
    };
  }

  test("collapses are sealed per account and read back in order", async () => {
    const sql = memSql();
    const store = createPluginStore(sql, { key: randomBytes(32) });
    await store.saveContextCollapse("u1", { from: "aaaaaa", to: "bbbbbb", summary: "secret summary one" });
    await store.saveContextCollapse("u1", { from: "cccccc", to: "dddddd", summary: "two" });
    const got = (await store.contextCollapses("u1")).sort((x, y) => x.from.localeCompare(y.from));
    expect(got).toEqual([
      { from: "aaaaaa", to: "bbbbbb", summary: "secret summary one" },
      { from: "cccccc", to: "dddddd", summary: "two" },
    ]);
    expect(await store.contextCollapses("u2")).toEqual([]);
    const raw = await sql.all("SELECT summary FROM chat_context_collapses");
    expect(raw.some((r) => String(r.summary).includes("secret summary"))).toBe(false);
  });

  test("without a key nothing is stored", async () => {
    const store = createPluginStore(memSql(), { key: null });
    await store.saveContextCollapse("u1", { from: "aaaaaa", to: "bbbbbb", summary: "s" });
    expect(await store.contextCollapses("u1")).toEqual([]);
  });
});
