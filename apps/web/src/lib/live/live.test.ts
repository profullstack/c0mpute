import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import type { AggregatorJob } from "../earnings/store";
import { fileReceipts, settlePass, type Charge } from "./settle";
import { bytesForUsd, createLiveStore, hashKey, LiveError, MAX_SPEND_USD_PER_HOUR, parseCreate, parseRelayJobId, slugName, streamJson } from "./store";

function mem(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => void db.query(q).run(...(p as never[])),
  };
}

const user = { id: "u1", coinpay_sub: "sub-1" };
const NET = "a".repeat(64);
const job = (over: Partial<AggregatorJob> = {}): AggregatorJob => ({ job_id: `live:${NET}:12D3KooWabc:100`, node: "12D3KooWabc", type: "live.relay", completed_at_ms: 1_000, seen_at_ms: 1_000, busy_s: 60, paid_usd: 0.02, ...over });

async function onAir(now = () => 5_000) {
  const store = createLiveStore(mem(), { now });
  const { stream, stream_key } = await store.create(user, { name: "Friday Show" });
  await store.ingestEvent({ id: stream.id, event: "start", stream_id: NET, peer: "12D3KooWabcdefgh" });
  return { store, stream, stream_key };
}

describe("names and ids", () => {
  test("slugName", () => {
    expect(slugName("  My Board Meeting!  ")).toBe("my-board-meeting");
    expect(slugName("ümlaut & co")).toBe("umlaut-co");
  });
  test("parseCreate refuses an empty name and a bad visibility", () => {
    expect(() => parseCreate({ name: "!!!" })).toThrow(LiveError);
    expect(() => parseCreate({ name: "a", visibility: "private" })).toThrow(LiveError);
    expect(parseCreate({ name: "A b", title: " T " })).toEqual({ name: "a-b", title: "T", visibility: "public" });
  });
  test("parseRelayJobId", () => {
    expect(parseRelayJobId(`live:${NET}:12D3KooWabc:42`)).toEqual({ network_id: NET, peer: "12D3KooWabc", epoch: 42 });
    expect(parseRelayJobId("job-123")).toBeNull();
    expect(parseRelayJobId(`live:${NET}:has0zero:1`)).toBeNull();
  });
  test("bytesForUsd: $0.01 is one GB", () => {
    expect(bytesForUsd(0.01)).toBe(1e9);
  });
});

describe("streams", () => {
  test("create returns the key once, stores only its hash, and never lists it", async () => {
    const sql = mem();
    const store = createLiveStore(sql);
    const { stream, stream_key } = await store.create(user, { name: "show" });
    expect(stream_key).toMatch(/^c0l_/);
    expect(stream.id).toMatch(/^lv_/);
    const [row] = await sql.all("SELECT key_hash FROM live_streams WHERE id = ?", [stream.id]);
    expect(row.key_hash).toBe(hashKey(stream_key));
    expect(JSON.stringify(await store.list(user.id))).not.toContain(stream_key);
    expect(JSON.stringify(streamJson(stream, "https://c0mpute.com"))).not.toContain("c0l_");
  });

  test("the same name twice on one account is a 409; another account may use it", async () => {
    const store = createLiveStore(mem());
    await store.create(user, { name: "show" });
    await expect(store.create(user, { name: "Show" })).rejects.toMatchObject({ status: 409 });
    await store.create({ id: "u2", coinpay_sub: null }, { name: "show" });
  });

  test("byKey finds the stream; a rotated or deleted key stops working", async () => {
    const store = createLiveStore(mem());
    const { stream, stream_key } = await store.create(user, { name: "show" });
    expect((await store.byKey(stream_key))?.id).toBe(stream.id);
    expect(await store.byKey("c0l_nope")).toBeNull();
    expect(await store.byKey(42)).toBeNull();
    const next = await store.rotateKey(user.id, stream.id);
    expect(await store.byKey(stream_key)).toBeNull();
    expect((await store.byKey(next))?.id).toBe(stream.id);
    expect(await store.rotateKey("someone-else", stream.id)).toBeNull();
    expect(await store.remove("someone-else", stream.id)).toBe(false);
    expect(await store.remove(user.id, stream.id)).toBe(true);
    expect(await store.byKey(next)).toBeNull();
    expect(await store.publicGet(stream.id)).toBeNull();
  });

  test("ingest start and end flip live, and only public live streams are listed on air", async () => {
    const { store, stream } = await onAir();
    expect((await store.publicGet(stream.id))?.live).toBe(true);
    expect((await store.onAir()).map((s) => s.id)).toEqual([stream.id]);
    const { stream: hidden } = await store.create(user, { name: "hidden", visibility: "unlisted" });
    await store.ingestEvent({ id: hidden.id, event: "start", stream_id: "b".repeat(64), peer: "12D3KooWabcdefgh" });
    expect((await store.onAir()).map((s) => s.id)).toEqual([stream.id]);
    await store.ingestEvent({ id: stream.id, event: "end" });
    expect((await store.publicGet(stream.id))?.live).toBe(false);
    await expect(store.ingestEvent({ id: stream.id, event: "start" })).rejects.toThrow(LiveError);
    expect(await store.ingestEvent({ id: "lv_doesnotexist00", event: "end" })).toBe(false);
  });
});

describe("settlement", () => {
  const ok: Charge = async () => ({ charged: true });

  test("a receipt is filed once against its stream; other job types are ignored", async () => {
    const { store, stream } = await onAir();
    expect(await fileReceipts([job(), job(), job({ type: "infernet.inference", job_id: "x" }), job({ job_id: "live:unknownstream0000:12D3KooWabc:1" })], store)).toBe(1);
    expect(await fileReceipts([job()], store)).toBe(0);
    const u = await store.usage(stream.id);
    expect(u.bytes).toBe(2e9);
    expect(u.cost_usd).toBeCloseTo(0.02);
    expect(u.charged_usd).toBe(0);
  });

  test("one receipt can charge at most the cap", async () => {
    const { store, stream } = await onAir();
    await fileReceipts([job({ paid_usd: 900 })], store);
    expect((await store.usage(stream.id)).cost_usd).toBe(0.5);
  });

  test("a charged receipt is charged once, with its job id as the idempotency key", async () => {
    const { store, stream } = await onAir();
    await fileReceipts([job()], store);
    const calls: unknown[] = [];
    const charge: Charge = async (subject, body) => {
      calls.push({ subject, ...body });
      return { charged: true };
    };
    expect(await settlePass(store, charge, 2_000)).toEqual({ charged: 1, unpaid: 0, retry: 0 });
    expect(await settlePass(store, charge, 2_000)).toEqual({ charged: 0, unpaid: 0, retry: 0 });
    expect(calls).toEqual([{ subject: "sub-1", idem_key: job().job_id, kind: "live.relay", cost_usd: 0.02, provider: "c0mpute:12D3KooWabc", meta: { stream: stream.id, bytes: 2e9 } }]);
    expect((await store.usage(stream.id)).charged_usd).toBeCloseTo(0.02);
  });

  test("an empty balance closes the receipt unpaid and drops the stream to the free tier", async () => {
    const { store, stream } = await onAir();
    await store.setPaid(stream.id, true);
    await fileReceipts([job()], store);
    expect(await settlePass(store, async () => ({ charged: false, reason: "insufficient_balance" }), 2_000)).toEqual({ charged: 0, unpaid: 1, retry: 0 });
    expect((await store.publicGet(stream.id))?.paid).toBe(false);
    expect(await settlePass(store, ok, 2_000)).toEqual({ charged: 0, unpaid: 0, retry: 0 });
  });

  test("Infernet unreachable leaves the receipt open for the next pass", async () => {
    const { store } = await onAir();
    await fileReceipts([job()], store);
    const down: Charge = async () => {
      throw new Error("down");
    };
    expect(await settlePass(store, down, 2_000)).toEqual({ charged: 0, unpaid: 0, retry: 1 });
    expect(await settlePass(store, ok, 2_000)).toEqual({ charged: 1, unpaid: 0, retry: 0 });
  });

  test("an account with no CoinPay identity is never charged", async () => {
    const store = createLiveStore(mem());
    const { stream } = await store.create({ id: "u3", coinpay_sub: null }, { name: "show" });
    await store.ingestEvent({ id: stream.id, event: "start", stream_id: NET, peer: "12D3KooWabcdefgh" });
    await fileReceipts([job()], store);
    let asked = 0;
    const count: Charge = async () => {
      asked++;
      return { charged: true };
    };
    expect(await settlePass(store, count, 2_000)).toEqual({ charged: 0, unpaid: 1, retry: 0 });
    expect(asked).toBe(0);
  });

  test("the hourly cap stops charging", async () => {
    const { store } = await onAir();
    const n = Math.ceil(MAX_SPEND_USD_PER_HOUR / 0.5) + 2;
    await fileReceipts(
      Array.from({ length: n }, (_, i) => job({ job_id: `live:${NET}:12D3KooWabc:${i}`, paid_usd: 0.5, completed_at_ms: 1_000 + i })),
      store,
    );
    const r = await settlePass(store, ok, 2_000);
    expect(r.charged).toBe(MAX_SPEND_USD_PER_HOUR / 0.5);
    expect(r.unpaid).toBe(2);
  });
});
