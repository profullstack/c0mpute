import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createShareStore, parseShareBody, ShareError } from "./share-store";
import type { Sql } from "./store";

function mem(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => void db.query(q).run(...(p as never[])),
  };
}

const chat = (over: Record<string, unknown> = {}) => ({
  convo_id: "abc123",
  title: "new chat",
  messages: [
    { role: "user", content: "2BR rent in the Marina?" },
    { role: "assistant", content: "// searching the web…" },
    {
      role: "assistant",
      content: "About **$5,000** [1].",
      sources: [
        { n: 1, title: "Rentable", url: "https://rentable.co/x", host: "rentable.co", image: "http://insecure/x.jpg", snippet: "s" },
        { n: 2, title: "bad", url: "javascript:alert(1)", host: "x" },
      ],
      run: "run_x",
      image: "blob:abc",
    },
    { role: "system", content: "secret system prompt" },
  ],
  ...over,
});

describe("shared chats", () => {
  test("a share keeps only question and answer text and safe sources", () => {
    const s = parseShareBody(chat());
    expect(s.title).toBe("2BR rent in the Marina?");
    expect(s.messages).toEqual([
      { role: "user", content: "2BR rent in the Marina?" },
      { role: "assistant", content: "About **$5,000** [1].", sources: [{ n: 1, title: "Rentable", url: "https://rentable.co/x", host: "rentable.co", image: null, snippet: "s" }] },
    ]);
    expect(() => parseShareBody(chat({ messages: [{ role: "user", content: "hi" }] }))).toThrow(ShareError);
    expect(() => parseShareBody({ messages: [] })).toThrow(/convo_id/);
  });

  test("anyone reads a share; sharing the same chat again keeps the link; only the owner unshares", async () => {
    let n = 0;
    const store = createShareStore(mem(), { newId: () => `share_id_${++n}xx` });
    const a = await store.put("mom", chat());
    expect(a.created).toBe(true);
    const got = await store.get(a.id);
    expect(got?.messages).toHaveLength(2);
    const again = await store.put("mom", chat({ messages: [...chat().messages, { role: "user", content: "and Sausalito?" }] }));
    expect(again).toEqual({ id: a.id, created: false });
    expect((await store.get(a.id))?.messages).toHaveLength(3);
    expect(await store.forConvo("mom", "abc123")).toBe(a.id);
    await store.view(a.id);
    expect((await store.list("mom"))[0]?.views).toBe(1);
    expect(await store.revoke("someone-else", a.id)).toBe(false);
    expect(await store.get(a.id)).not.toBeNull();
    expect(await store.revoke("mom", a.id)).toBe(true);
    expect(await store.get(a.id)).toBeNull();
    const fresh = await store.put("mom", chat());
    expect(fresh.id).not.toBe(a.id);
  });

  test("ids that do not look like share ids are never queried", async () => {
    const store = createShareStore(mem());
    expect(await store.get("../../etc")).toBeNull();
    expect(await store.get("short")).toBeNull();
  });
});
