import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";
import type { Sql } from "./store";
import { canonicalLabel, labelerFromEnv, moderatePush, THREATCRUSH, verifyLabel, type Label, type LabelerConfig } from "./moderation";
import { createPushStore, PushError } from "./push";

// RFC 8032 test vector 1, so the signing here is checkable by hand.
const SEED = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const priv = createPrivateKey({ key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), Buffer.from(SEED, "hex")]), format: "der", type: "pkcs8" });
const spki = createPublicKey(priv).export({ format: "der", type: "spki" });
const PUB = Buffer.from(spki.subarray(spki.length - 32)).toString("base64url");
const DID = "did:web:labeler.test";
const sha = (s: string) => createHash("sha256").update(s, "utf8").digest("hex");
const iso = (ms: number) => new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");

function label(over: Partial<Label> = {}, signWith = priv): Label {
  const base = { src: DID, uri: `sha256:${sha("<h1>evil</h1>")}`, val: "phishing", neg: false, cts: iso(Date.now() - 1000), exp: iso(Date.now() + 86_400_000), ...over };
  const msg = canonicalLabel(base)!;
  return { kind: "label", ver: 1, ...base, sig: sign(null, Buffer.from(msg), signWith).toString("base64url"), ...("sig" in over ? { sig: over.sig! } : {}) };
}

const cfg: LabelerConfig = { url: "https://labeler.test", did: DID, publicKey: PUB, token: "t".repeat(32), refuse: new Set(["malware", "phishing", "cryptominer"]), appeal: "appeals@labeler.test", timeoutMs: 1000 };
const evil = [{ path: "index.html", type: "text/html", text: "<h1>evil</h1>" }];
const clean = [{ path: "index.html", type: "text/html", text: "<h1>hi</h1>" }];

type Call = { url: string; init: RequestInit };
function labeler(labels: unknown, status = 200) {
  const calls: Call[] = [];
  const f = (async (url: string, init: RequestInit) => {
    calls.push({ url, init });
    return new Response(JSON.stringify({ labels }), { status, headers: { "content-type": "application/json" } });
  }) as unknown as typeof fetch;
  return { f, calls };
}

describe("labeler config", () => {
  test("off without a scan token or when switched off; ThreatCrush pinned by default", () => {
    expect(labelerFromEnv({})).toBeNull();
    expect(labelerFromEnv({ C0MPUTE_LABELER_TOKEN: "x", C0MPUTE_MODERATION: "off" })).toBeNull();
    const c = labelerFromEnv({ C0MPUTE_LABELER_TOKEN: "tok" })!;
    expect(c).toMatchObject({ url: THREATCRUSH.url, did: THREATCRUSH.did, publicKey: THREATCRUSH.publicKey });
    expect([...c.refuse].sort()).toEqual(["cryptominer", "malware", "phishing"]);
    expect([...labelerFromEnv({ C0MPUTE_LABELER_TOKEN: "t", C0MPUTE_MODERATION_REFUSE: "malware" })!.refuse]).toEqual(["malware"]);
  });
});

describe("label verification", () => {
  test("RFC 8032 public key, round trip, and tamper refusal", () => {
    expect(Buffer.from(PUB, "base64url").toString("hex")).toBe("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    const l = label();
    expect(verifyLabel(l, PUB)).toBe(true);
    expect(verifyLabel({ ...l, val: "malware" }, PUB)).toBe(false);
    expect(verifyLabel({ ...l, neg: true }, PUB)).toBe(false);
    expect(verifyLabel({ ...l, uri: `sha256:${sha("x")}` }, PUB)).toBe(false);
    expect(verifyLabel({ ...l, sig: "" }, PUB)).toBe(false);
    expect(verifyLabel(l, "AAAA")).toBe(false);
    expect(verifyLabel({ ...l, src: `${DID}\nx` }, PUB)).toBe(false);
  });
  test("framing matches the ThreatCrush labeler byte for byte", () => {
    expect(canonicalLabel({ src: "did:web:threatcrush.com", uri: `sha256:${"a".repeat(64)}`, val: "malware", neg: false, cts: "2026-11-22T00:00:00Z", exp: "2026-12-22T00:00:00Z" })).toBe(
      `c0mpute-label-v1\ndid:web:threatcrush.com\nsha256:${"a".repeat(64)}\nmalware\n0\n2026-11-22T00:00:00Z\n2026-12-22T00:00:00Z`,
    );
  });
});

describe("moderatePush", () => {
  test("refuses a push carrying a valid label for one of its files, with reason and appeal", async () => {
    const { f, calls } = labeler([label()]);
    const v = await moderatePush(evil, cfg, f);
    expect(v.verdict).toBe("refused");
    if (v.verdict === "refused") {
      expect(v.labels).toEqual([{ path: "index.html", val: "phishing", evidence: undefined }]);
      expect(v.message).toContain("index.html (phishing)");
      expect(v.message).toContain("appeals@labeler.test");
    }
    expect(calls[0].url).toBe("https://labeler.test/api/v1/labels/scan");
    expect((calls[0].init.headers as Record<string, string>).authorization).toBe(`Bearer ${cfg.token}`);
    const sent = JSON.parse(String(calls[0].init.body));
    expect(Buffer.from(sent.files[0].content_b64, "base64").toString()).toBe("<h1>evil</h1>");
  });

  test("ignores every label that must not count", async () => {
    const other = await (await import("node:crypto")).generateKeyPairSync("ed25519").privateKey;
    const bad = [
      label({ uri: `sha256:${sha("some other site's file")}` }), // replay of a real label for a file we did not send
      label({ src: "did:web:evil.example" }), // wrong labeler
      label({ neg: true }), // a retraction
      label({ exp: iso(Date.now() - 1000), cts: iso(Date.now() - 5000) }), // expired
      label({ val: "spam" }), // a value this operator does not refuse
      label({}, other), // signed by someone else
      { ...label(), sig: label({ val: "malware" }).sig }, // signature lifted from another label
      { ...label(), neg: "false" }, // wrong type
      null,
      "label",
    ];
    for (const l of bad) expect((await moderatePush(evil, cfg, labeler([l]).f)).verdict).toBe("ok");
    expect((await moderatePush(evil, cfg, labeler("not an array").f)).verdict).toBe("ok");
  });

  test("a clean site passes, and a site with no inline files is not sent anywhere", async () => {
    expect(await moderatePush(clean, cfg, labeler([]).f)).toEqual({ verdict: "ok", checked: 1 });
    const { f, calls } = labeler([]);
    expect(await moderatePush([{ path: "a.png", type: "image/png", src: "/api/v1/images/x" }], cfg, f)).toEqual({ verdict: "ok", checked: 0 });
    expect(calls).toHaveLength(0);
  });

  test("fails open: labeler errors, outages, timeouts and junk never block a push", async () => {
    expect((await moderatePush(evil, cfg, labeler([label()], 503).f)).verdict).toBe("unchecked");
    expect((await moderatePush(evil, cfg, labeler([label()], 401).f)).verdict).toBe("unchecked");
    const down = (async () => {
      throw new TypeError("fetch failed");
    }) as unknown as typeof fetch;
    expect(await moderatePush(evil, cfg, down)).toEqual({ verdict: "unchecked", reason: "labeler unreachable: TypeError" });
    const junk = (async () => new Response("<html>", { status: 200 })) as unknown as typeof fetch;
    expect((await moderatePush(evil, cfg, junk)).verdict).toBe("unchecked");
    const slow = ((_u: string, init: RequestInit) =>
      new Promise((_, rej) => init.signal?.addEventListener("abort", () => rej(new DOMException("timeout", "TimeoutError"))))) as unknown as typeof fetch;
    expect(await moderatePush(evil, { ...cfg, timeoutMs: 20 }, slow)).toEqual({ verdict: "unchecked", reason: "labeler unreachable: TimeoutError" });
  });
});

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

describe("cloud node push store with moderation", () => {
  async function setup(moderate: Parameters<typeof createPushStore>[1]["moderate"]) {
    const sql = memSql();
    const house = await createPushStore(sql).registerNode("house", { name: "cloud-vienna", payments: "off" });
    const s = createPushStore(sql, { cloudNodeId: house.id, moderate });
    return { s, cloud: (await s.cloudNode())! };
  }

  test("a refused push is 451 and writes nothing", async () => {
    const { s, cloud } = await setup((files) => moderatePush(files, cfg, labeler([label()]).f));
    const err = await s.upsertPush("u1", { target: "hosting", name: "bank-login", node: cloud, files: evil }).catch((e) => e);
    expect(err).toBeInstanceOf(PushError);
    expect((err as PushError).status).toBe(451);
    expect(await s.push("u1", "hosting", "bank-login")).toBeNull();
  });

  test("an update to an existing site is checked too", async () => {
    let flagged = false;
    const { s, cloud } = await setup((files) => moderatePush(files, cfg, labeler(flagged ? [label()] : []).f));
    await s.upsertPush("u1", { target: "hosting", name: "blog", node: cloud, files: clean });
    flagged = true;
    await expect(s.upsertPush("u1", { target: "hosting", name: "blog", node: cloud, files: evil })).rejects.toThrow(/content policy/);
    expect((await s.push("u1", "hosting", "blog"))!.files[0].text).toBe("<h1>hi</h1>");
  });

  test("clean pushes and labeler outages go through", async () => {
    const down = (async () => {
      throw new TypeError("fetch failed");
    }) as unknown as typeof fetch;
    const ok = await setup((files) => moderatePush(files, cfg, labeler([]).f));
    expect((await ok.s.upsertPush("u1", { target: "hosting", name: "a", node: ok.cloud, files: clean })).status).toBe("queued");
    const out = await setup((files) => moderatePush(files, cfg, down));
    expect((await out.s.upsertPush("u1", { target: "hosting", name: "b", node: out.cloud, files: evil })).status).toBe("queued");
  });

  test("pushes to a person's own node are their operator's call and are not checked", async () => {
    let called = 0;
    const { s } = await setup(async () => {
      called++;
      return { verdict: "refused", labels: [], message: "no" };
    });
    const mine = await s.registerNode("u1", { name: "seed2", payments: "coinpay" });
    expect((await s.upsertPush("u1", { target: "hosting", name: "x", node: mine, files: evil })).status).toBe("queued");
    expect(called).toBe(0);
  });
});
