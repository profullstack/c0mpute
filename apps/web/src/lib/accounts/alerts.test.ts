import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import {
  composeNotice,
  createAlertStore,
  DEFAULT_ALERTS,
  evaluateAccount,
  findings,
  patchSettings,
  stateKey,
  transitions,
  type AlertSettings,
  type EvalDeps,
} from "./alerts";
import type { FleetNode } from "./fleet";

function memSql(db = new Database(":memory:")): Sql {
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const T0 = Date.parse("2026-11-22T12:00:00.000Z");
const MIN = 60_000;

function fnode(over: Partial<FleetNode> = {}): FleetNode {
  return {
    node: "AbCd1234",
    peer_id: "12D3KooWxxxxAbCd1234",
    label: "rack-a",
    tags: [],
    verified: true,
    via: "signed",
    online: true,
    last_seen_s: 10,
    version: "0.3.106",
    class: "cpu",
    cores: 8,
    ram_bytes: 16e9,
    storage: { free_bytes: 200e9, total_bytes: 500e9 },
    info: null,
    jobs_24h: 3,
    earned_usd: { h24: 1, d30: 30, all: 100 },
    cost_usd_per_hour: null,
    paused: false,
    pending_commands: 0,
    linked_at: "2026-11-01T00:00:00.000Z",
    node_state: null,
    state_at: null,
    ...over,
  };
}

const settings = (over: Partial<AlertSettings> = {}): AlertSettings => ({ ...DEFAULT_ALERTS, kinds: { ...DEFAULT_ALERTS.kinds }, email: "ops@example.com", email_verified: true, ...over });

/** A fake world: the fleet is whatever `nodes` holds at the moment of each pass. */
function world() {
  const sql = memSql();
  let now = T0;
  const store = createAlertStore(sql, () => now);
  const sent: { to: string; subject: string; lines: string[] }[] = [];
  const sms: string[] = [];
  const w = {
    sql,
    store,
    sent,
    sms,
    nodes: [fnode()],
    boardOk: true,
    earnings: { h24: 1, avg7: 1 },
    updates: new Map<string, { status: string; done_at: string | null; result: string | null }>(),
    emailFails: false,
    smsAllowed: false,
    get now() {
      return now;
    },
    set now(v: number) {
      now = v;
    },
  };
  const deps: EvalDeps = {
    store,
    fleet: async () => ({ nodes: w.nodes, boardOk: w.boardOk }),
    earnings: async () => w.earnings,
    updates: async () => w.updates,
    channels: {
      email: async (to, subject, lines) => {
        if (w.emailFails) throw new Error("smtp down");
        sent.push({ to, subject, lines });
      },
      sms: async (_u, message) => {
        if (!w.smsAllowed) return false;
        sms.push(message);
        return true;
      },
    },
    site: "https://c0mpute.com",
    now: () => now,
  };
  return { w, deps };
}

describe("settings", () => {
  test("patches are checked and merged; a new email needs confirming", () => {
    const cur = settings({ email: null, email_verified: false });
    const p = patchSettings(cur, { email: "Me@Example.com", offline_minutes: 20, kinds: { earnings: false } });
    if (typeof p === "string") throw new Error(p);
    expect(p.emailChanged).toBe(true);
    expect(p.next).toMatchObject({ email: "Me@Example.com", email_verified: false, offline_minutes: 20 });
    expect(p.next.kinds).toEqual({ offline: true, earnings: false, update: true, disk: true });
    // same address, other case: no new confirmation
    const same = patchSettings({ ...p.next, email_verified: true }, { email: "me@example.com" });
    expect(typeof same !== "string" && same.emailChanged).toBe(false);
    expect(typeof patchSettings(cur, { email: "not an email" })).toBe("string");
    expect(typeof patchSettings(cur, { email: "a@b.c\r\nBcc: x@y.z" })).toBe("string");
    expect(typeof patchSettings(cur, { offline_minutes: 1 })).toBe("string");
    expect(typeof patchSettings(cur, { kinds: { weather: true } })).toBe("string");
    expect(typeof patchSettings(cur, { sms: "yes" })).toBe("string");
  });

  test("an address only counts after its link is opened; links are one-shot and expire", async () => {
    const { w } = world();
    const cur = await w.store.get("u1");
    expect(cur.email).toBeNull();
    const p = patchSettings(cur, { email: "ops@example.com" });
    if (typeof p === "string") throw new Error(p);
    const { verifyToken } = await w.store.save("u1", p.next, p.emailChanged);
    expect(verifyToken?.startsWith("c0v_")).toBe(true);
    expect((await w.store.get("u1")).email_verified).toBe(false);
    expect(await w.store.recipients()).toHaveLength(0);
    // stored hashed
    expect(JSON.stringify(await w.sql.all(`SELECT * FROM fleet_alert_settings`))).not.toContain(verifyToken!);
    expect(await w.store.verify("c0v_wrong")).toBeNull();
    expect(await w.store.verify(verifyToken!)).toEqual({ user_id: "u1", email: "ops@example.com" });
    expect(await w.store.verify(verifyToken!)).toBeNull();
    expect((await w.store.get("u1")).email_verified).toBe(true);
    expect(await w.store.recipients()).toHaveLength(1);
    // a new address drops the confirmation; an old link for it expires
    const p2 = patchSettings(await w.store.get("u1"), { email: "new@example.com" });
    if (typeof p2 === "string") throw new Error(p2);
    const t2 = (await w.store.save("u1", p2.next, p2.emailChanged)).verifyToken!;
    expect((await w.store.get("u1")).email_verified).toBe(false);
    w.now = T0 + 49 * 3_600_000;
    expect(await w.store.verify(t2)).toBeNull();
  });
});

describe("findings", () => {
  const base = { settings: settings(), boardOk: true, earnings: null, updates: new Map(), active: new Set<string>(), now: T0 };
  test("offline after N minutes by the fresher of the board and the node's own poll", () => {
    const off = (n: FleetNode) => findings({ ...base, nodes: [n] }).find((f) => f.kind === "offline")!;
    expect(off(fnode()).firing).toBe(false);
    expect(off(fnode({ online: false, last_seen_s: 16 * 60 })).firing).toBe(true);
    expect(off(fnode({ online: false, last_seen_s: 10 * 60 })).firing).toBe(false);
    // off the board, but polling us a minute ago: online enough
    expect(off(fnode({ online: false, last_seen_s: null, state_at: new Date(T0 - MIN).toISOString() })).firing).toBe(false);
    expect(off(fnode({ online: false, last_seen_s: null, state_at: new Date(T0 - 30 * MIN).toISOString() })).detail).toContain("30 min");
    // the board could not be read: no offline verdicts at all
    expect(findings({ ...base, boardOk: false, nodes: [fnode({ online: false, last_seen_s: 9999 })] }).some((f) => f.kind === "offline")).toBe(false);
  });

  test("disk low by GB or %, clearing only above the margin", () => {
    const disk = (free: number, active = false) =>
      findings({ ...base, active: active ? new Set([stateKey("AbCd1234", "disk")]) : new Set(), nodes: [fnode({ storage: { free_bytes: free, total_bytes: 1000e9 } })] }).find((f) => f.kind === "disk")!.firing;
    expect(disk(200e9)).toBe(false);
    expect(disk(9e9)).toBe(true); // under 10 GB (and under 5%)
    expect(disk(40e9)).toBe(true); // 4% free
    expect(disk(52e9, true)).toBe(true); // 5.2% < 5.5% while open
    expect(disk(56e9, true)).toBe(false);
    // the node's own report beats the board
    const n = fnode({ storage: { free_bytes: 900e9, total_bytes: 1000e9 }, node_state: { v: 1, version: null, paused: null, settings: {} as never, effective: {} as never, disk: { free_bytes: 1e9, total_bytes: 1000e9 }, update_error: null } });
    expect(findings({ ...base, nodes: [n] }).find((f) => f.kind === "disk")!.firing).toBe(true);
  });

  test("update failed: a failed fleet update this week, or the node's own failed install", () => {
    const upd = (u: Map<string, { status: string; done_at: string | null; result: string | null }>, n = fnode()) => findings({ ...base, updates: u, nodes: [n] }).find((f) => f.kind === "update")!;
    expect(upd(new Map()).firing).toBe(false);
    expect(upd(new Map([["AbCd1234", { status: "failed", done_at: new Date(T0 - 3_600_000).toISOString(), result: "update failed: disk full" }]])).detail).toContain("disk full");
    expect(upd(new Map([["AbCd1234", { status: "failed", done_at: "2026-11-01T00:00:00.000Z", result: "old" }]])).firing).toBe(false);
    expect(upd(new Map([["AbCd1234", { status: "done", done_at: new Date(T0).toISOString(), result: "updated" }]])).firing).toBe(false);
    const n = fnode({ node_state: { v: 1, version: "0.3.105", paused: false, settings: {} as never, effective: {} as never, disk: null, update_error: { version: "0.3.106", error: "signature mismatch", at: 1 } } });
    expect(upd(new Map(), n)).toMatchObject({ firing: true, detail: "update to 0.3.106 failed: signature mismatch" });
  });

  test("earnings drop: last 24h against the 7-day daily average, ignored for tiny averages", () => {
    const e = (h24: number, avg7: number, s = settings()) => findings({ ...base, settings: s, earnings: { h24, avg7 }, nodes: [fnode()] }).find((f) => f.kind === "earnings")!;
    expect(e(1, 1).firing).toBe(false);
    expect(e(0.4, 1)).toMatchObject({ firing: true, node: "*" });
    expect(e(0.4, 1).detail).toContain("60% down");
    expect(e(0.6, 1).firing).toBe(false);
    expect(e(0.6, 1, settings({ earnings_drop_pct: 30 })).firing).toBe(true);
    expect(e(0, 0.01).firing).toBe(false); // under earnings_min_usd
  });
});

describe("one outage = one alert + one recovery", () => {
  test("offline opens once, stays quiet while open, recovers once", async () => {
    const { w, deps } = world();
    const s = settings();
    w.nodes = [fnode({ online: false, last_seen_s: 20 * 60 })];
    expect(await evaluateAccount("u1", s, deps)).toHaveLength(1);
    expect(w.sent).toHaveLength(1);
    expect(w.sent[0].subject).toBe("c0mpute alert: node AbCd1234 (rack-a) offline");
    expect(w.sent[0].to).toBe("ops@example.com");
    // still down for an hour of ticks: nothing more
    for (let i = 1; i <= 60; i++) {
      w.now = T0 + i * MIN;
      w.nodes = [fnode({ online: false, last_seen_s: 20 * 60 + i * 60 })];
      await evaluateAccount("u1", s, deps);
    }
    expect(w.sent).toHaveLength(1);
    // back
    w.now += MIN;
    w.nodes = [fnode()];
    await evaluateAccount("u1", s, deps);
    expect(w.sent).toHaveLength(2);
    expect(w.sent[1].subject).toBe("c0mpute recovered: node AbCd1234 (rack-a) back online");
    await evaluateAccount("u1", s, deps);
    expect(w.sent).toHaveLength(2);
    const log = await w.store.history("u1");
    expect(log.map((l) => l.event)).toEqual(["recovery", "alert"]);
    expect(log.every((l) => l.ok && l.channels.join() === "email")).toBe(true);
  });

  test("a flapping node within the cooldown sends nothing more; still down after the cooldown, it alerts once", async () => {
    const { w, deps } = world();
    const s = settings({ cooldown_minutes: 60 });
    const down = () => (w.nodes = [fnode({ online: false, last_seen_s: 30 * 60 })]);
    const up = () => (w.nodes = [fnode()]);
    down();
    await evaluateAccount("u1", s, deps); // alert
    w.now += MIN;
    up();
    await evaluateAccount("u1", s, deps); // recovery
    expect(w.sent).toHaveLength(2);
    // flaps three times in the next 30 minutes: held back, and no recovery notices for unsent alerts
    for (let i = 0; i < 3; i++) {
      w.now += 5 * MIN;
      down();
      await evaluateAccount("u1", s, deps);
      w.now += 5 * MIN;
      up();
      await evaluateAccount("u1", s, deps);
    }
    expect(w.sent).toHaveLength(2);
    // goes down and stays down past the cooldown: one alert when the cooldown ends
    w.now += MIN;
    down();
    await evaluateAccount("u1", s, deps);
    expect(w.sent).toHaveLength(2);
    w.now = T0 + 2 * MIN + 62 * MIN;
    await evaluateAccount("u1", s, deps);
    expect(w.sent).toHaveLength(3);
    expect(w.sent[2].subject).toContain("offline");
  });

  test("many nodes failing in one tick is one email", async () => {
    const { w, deps } = world();
    w.nodes = [fnode({ node: "AAAAAAAA", online: false, last_seen_s: 3600 }), fnode({ node: "BBBBBBBB", online: false, last_seen_s: 3600, storage: { free_bytes: 1e9, total_bytes: 100e9 } })];
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(1);
    expect(w.sent[0].subject).toBe("c0mpute fleet: 3 alerts");
    expect(w.sent[0].lines.filter((l) => l.startsWith("ALERT"))).toHaveLength(3);
  });

  test("an unreadable board leaves open offline incidents alone instead of recovering them", async () => {
    const { w, deps } = world();
    w.nodes = [fnode({ online: false, last_seen_s: 3600 })];
    await evaluateAccount("u1", settings(), deps);
    w.boardOk = false;
    w.now += MIN;
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(1);
    expect((await w.store.states("u1")).get(stateKey("AbCd1234", "offline"))?.active).toBe(true);
  });

  test("a failed send is retried next tick; nothing is lost and nothing doubles", async () => {
    const { w, deps } = world();
    w.nodes = [fnode({ online: false, last_seen_s: 3600 })];
    w.emailFails = true;
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(0);
    expect((await w.store.history("u1"))[0]).toMatchObject({ ok: false, event: "alert" });
    w.emailFails = false;
    w.now += MIN;
    await evaluateAccount("u1", settings(), deps);
    w.now += MIN;
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(1);
  });

  test("turning a kind off closes its incident silently; an unlinked node's incident closes too", async () => {
    const { w, deps } = world();
    w.nodes = [fnode({ online: false, last_seen_s: 3600 })];
    await evaluateAccount("u1", settings(), deps);
    w.now += MIN;
    await evaluateAccount("u1", settings({ kinds: { offline: false, earnings: true, update: true, disk: true } }), deps);
    expect(w.sent).toHaveLength(1);
    expect((await w.store.states("u1")).get(stateKey("AbCd1234", "offline"))?.active).toBe(false);
    // open again, then the node leaves the account
    w.now += 2 * 3_600_000;
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(2);
    w.nodes = [];
    w.now += MIN;
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent).toHaveLength(2);
    expect([...(await w.store.states("u1")).values()].some((s) => s.active)).toBe(false);
  });

  test("SMS goes out only where the channel allows it (admin accounts), alongside email", async () => {
    const { w, deps } = world();
    w.nodes = [fnode({ online: false, last_seen_s: 3600 })];
    await evaluateAccount("u1", settings({ sms: true }), deps);
    expect(w.sms).toHaveLength(0); // not an admin account
    expect((await w.store.history("u1"))[0].channels).toEqual(["email"]);
    const { w: w2, deps: d2 } = world();
    w2.smsAllowed = true;
    w2.nodes = [fnode({ online: false, last_seen_s: 3600 })];
    await evaluateAccount("admin", settings({ sms: true }), d2);
    expect(w2.sms[0]).toContain("offline");
    expect((await w2.store.history("admin"))[0].channels).toEqual(["email", "sms"]);
  });

  test("earnings: one fleet-wide incident", async () => {
    const { w, deps } = world();
    w.earnings = { h24: 0.1, avg7: 2 };
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent[0].subject).toBe("c0mpute alert: your fleet earnings dropped");
    w.now += MIN;
    w.earnings = { h24: 1.8, avg7: 2 };
    await evaluateAccount("u1", settings(), deps);
    expect(w.sent[1].subject).toBe("c0mpute recovered: your fleet earnings recovered");
  });
});

describe("transitions and wording", () => {
  test("a recovery without a sent alert is silent", () => {
    const states = new Map([[stateKey("n", "disk"), { node: "n", kind: "disk" as const, active: true, notified: false, since: null, detail: "", last_alert_at: null, last_recovered_at: null }]]);
    const r = transitions({ settings: settings(), findings: [{ node: "n", label: null, kind: "disk", firing: false, detail: "ok" }], states, evaluated: new Set(["disk"]), fleetNodes: new Set(["n"]), now: T0 });
    expect(r.notices).toHaveLength(0);
    expect(r.writes[0].active).toBe(false);
  });
  test("the message names the node and links to the fleet and the settings", () => {
    const m = composeNotice([{ node: "AbCd1234", label: "rack-a", kind: "disk", event: "alert", detail: "4.0 GB free of 500.0 GB (0.8%)" }], "https://c0mpute.com");
    expect(m.subject).toBe("c0mpute alert: node AbCd1234 (rack-a) disk low");
    expect(m.lines.join("\n")).toContain("https://c0mpute.com/dashboard/nodes");
    expect(m.lines.join("\n")).toContain("/account#alerts");
    expect(m.sms.length).toBeLessThanOrEqual(300);
    expect(m.lines.join("\n")).not.toContain("—");
  });
  test("one evaluator at a time", async () => {
    const { w } = world();
    expect(await w.store.lease("a", 180_000)).toBe(true);
    expect(await w.store.lease("b", 180_000)).toBe(false);
    expect(await w.store.lease("a", 180_000)).toBe(true);
    w.now += 181_000;
    expect(await w.store.lease("b", 180_000)).toBe(true);
  });
});
