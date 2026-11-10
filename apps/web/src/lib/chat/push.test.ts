import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "./store";
import {
  cleanPath,
  collectPushFiles,
  createPushStore,
  nodeJob,
  parseConnectCommand,
  parsePushCommand,
  publicPush,
  PushError,
  validatePushFiles,
} from "./push";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const FILES = [{ path: "index.html", type: "text/html", text: "<h1>hi</h1>" }];

describe("commands", () => {
  test("/push hosting <name>, ls, rm, and the /push <name> shorthand", () => {
    expect(parsePushCommand("/push hosting seed2")).toEqual({ kind: "push", target: "hosting", name: "seed2" });
    expect(parsePushCommand("/push seed2")).toEqual({ kind: "push", target: "hosting", name: "seed2" });
    expect(parsePushCommand("/push hosting ls")).toEqual({ kind: "ls", target: "hosting" });
    expect(parsePushCommand("/push hosting rm seed2")).toEqual({ kind: "rm", target: "hosting", name: "seed2" });
    expect(parsePushCommand("/push")).toEqual({ kind: "help" });
    expect(parsePushCommand("/push hosting Bad_Name")?.kind).toBe("help");
    expect(parsePushCommand("push it")).toBeNull();
    expect(parsePushCommand("/pushy")).toBeNull();
  });

  test("/connect hosting [node]", () => {
    expect(parseConnectCommand("/connect hosting")).toEqual({ target: "hosting", node: null });
    expect(parseConnectCommand("/connect hosting seed2")).toEqual({ target: "hosting", node: "seed2" });
    expect(parseConnectCommand("/connect seed2")).toEqual({ target: "hosting", node: "seed2" });
    expect(parseConnectCommand("/connect hosting ../x")?.error).toBeTruthy();
  });
});

describe("files", () => {
  test("paths stay inside the site", () => {
    expect(cleanPath("music/01 - Song.mp3")).toBe("music/01 - Song.mp3");
    expect(cleanPath("/index.html")).toBe("index.html");
    for (const bad of ["../x", "a/../b", ".env", "a/.git/x", "a\\b", "", "a/b/c/d/e", "..", "a//b"]) expect(cleanPath(bad)).toBeNull();
  });

  test("only our own result routes may be referenced", () => {
    expect(validatePushFiles([{ path: "a.png", type: "image/png", src: "/api/v1/images/abc123" }])).toHaveLength(1);
    expect(validatePushFiles([{ path: "a.png", type: "image/png", src: "https://evil.example/x" }])).toBeTypeOf("string");
    expect(validatePushFiles([{ path: "a.png", type: "image/png", src: "/api/v1/keys" }])).toBeTypeOf("string");
    expect(validatePushFiles([{ path: "a", text: "x" }, { path: "A", text: "y" }])).toBeTypeOf("string");
    expect(validatePushFiles([{ path: "big.txt", text: "x".repeat(1024 * 1024 + 1) }])).toBeTypeOf("string");
    expect(validatePushFiles([])).toBeTypeOf("string");
  });

  test("collects every result in a chat plus the transcript", () => {
    const files = collectPushFiles("my chat", [
      { role: "user", content: "/image a cat" },
      { role: "assistant", content: "[image: a cat]", imageJob: "img1" },
      { role: "assistant", content: '"Neon Rain" [music: synthwave]', musicJob: "m1", audioType: "audio/mpeg" },
      { role: "assistant", content: "album", album: { id: "al1", band: "Rusty Waves", album: "Metal Tides", cover: "x", zip: "y" } },
      { role: "assistant", content: "promo", promo: { id: "pr1", video: "/v", captions: "/c" } },
    ]);
    const paths = files.map((f) => f.path);
    expect(paths).toEqual([
      "index.html",
      "chat.md",
      "images/01.png",
      "music/01 - Neon Rain.mp3",
      "music/01 - Neon Rain.png",
      "albums/Rusty Waves - Metal Tides.zip",
      "albums/Rusty Waves - Metal Tides.png",
      "video/01.mp4",
      "video/01.vtt",
    ]);
    expect(validatePushFiles(files)).toHaveLength(files.length);
    const html = files[0].text!;
    expect(html).toContain('src="images/01.png"');
    expect(html).toContain("music/01%20-%20Neon%20Rain.mp3");
    expect(html).not.toContain("<script");
  });

  test("the transcript escapes chat text", () => {
    const [html] = collectPushFiles("<b>t</b>", [{ role: "user", content: "<script>alert(1)</script>" }]);
    expect(html.text).not.toContain("<script>");
    expect(html.text).toContain("&lt;script&gt;");
  });
});

describe("push store", () => {
  async function setup() {
    const s = createPushStore(memSql());
    const node = await s.registerNode("u1", { name: "seed2", peer_id: "12D3", payments: "coinpay", version: "0.3.29" });
    return { s, node };
  }

  test("nodes and pushes are per account", async () => {
    const { s, node } = await setup();
    const p = await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    expect(p.status).toBe("queued");
    expect(await s.push("u2", "hosting", "site")).toBeNull();
    expect(await s.pushById("u2", p.id)).toBeNull();
    expect(await s.nodes("u2")).toEqual([]);
    await expect(s.claimJobs("u2", node.id)).rejects.toBeInstanceOf(PushError);
    await expect(s.report("u2", node.id, p.id, { rev: 1, ok: true, site_id: "abcdef12" })).rejects.toBeInstanceOf(PushError);
    // The same name under another account is a different push.
    const n2 = await s.registerNode("u2", { name: "seed2", payments: "coinpay" });
    const p2 = await s.upsertPush("u2", { target: "hosting", name: "site", node: n2, files: FILES });
    expect(p2.id).not.toBe(p.id);
  });

  test("claim -> progress -> live, then a re-push bumps rev and a stale report is refused", async () => {
    const { s, node } = await setup();
    const p = await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    const [job] = await s.claimJobs("u1", node.id);
    expect(job.status).toBe("publishing");
    expect(nodeJob(job)).toMatchObject({ action: "publish", rev: 1, files: [{ path: "index.html", text: "<h1>hi</h1>" }] });
    expect(await s.claimJobs("u1", node.id)).toEqual([]); // claimed: not handed out twice

    const mid = await s.report("u1", node.id, p.id, { rev: 1, progress: "  copies 1/3\u0007" });
    expect(mid !== "deleted" && mid.progress).toBe("copies 1/3");
    expect(mid !== "deleted" && publicPush(mid).progress).toBe("copies 1/3");

    const live = await s.report("u1", node.id, p.id, {
      rev: 1,
      ok: true,
      site_id: "abcdef1234",
      version: 7,
      url: "https://g/s/abcdef1234/",
      copies: 3,
      bytes: 99,
      note: "left out (expired on the network): images/01.png",
    });
    expect(live).toMatchObject({ status: "live", site_id: "abcdef1234", copies: 3, bytes: 99, progress: null, error: "left out (expired on the network): images/01.png" });

    const again = await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    expect(again.rev).toBe(2);
    await s.claimJobs("u1", node.id);
    await expect(s.report("u1", node.id, p.id, { rev: 1, ok: true, site_id: "abcdef1234" })).rejects.toThrow(/stale/);
  });

  test("a site stays with the node that signed it", async () => {
    const { s, node } = await setup();
    const other = await s.registerNode("u1", { name: "laptop", payments: "coinpay" });
    const p = await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    await s.claimJobs("u1", node.id);
    await s.report("u1", node.id, p.id, { rev: 1, ok: true, site_id: "abcdef1234" });
    await expect(s.upsertPush("u1", { target: "hosting", name: "site", node: other, files: FILES })).rejects.toThrow(/signed by node seed2/);
  });

  test("rm: unpublished goes at once; live goes through the node; a failed rm leaves it live", async () => {
    const { s, node } = await setup();
    await s.upsertPush("u1", { target: "hosting", name: "draft", node, files: FILES });
    expect(await s.deletePush("u1", "hosting", "draft")).toBe("gone");

    const p = await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    await s.claimJobs("u1", node.id);
    await s.report("u1", node.id, p.id, { rev: 1, ok: true, site_id: "abcdef1234" });
    expect(await s.deletePush("u1", "hosting", "site")).toBe("deleting");
    const [job] = await s.claimJobs("u1", node.id);
    expect(nodeJob(job)).toMatchObject({ action: "rm", name: "site", files: [] });
    expect(await s.report("u1", node.id, p.id, { rev: 1, ok: false, error: "timeout" })).toMatchObject({ status: "live", error: "delete failed: timeout" });

    await s.deletePush("u1", "hosting", "site");
    await s.claimJobs("u1", node.id);
    expect(await s.report("u1", node.id, p.id, { rev: 1, ok: true })).toBe("deleted");
    expect(await s.push("u1", "hosting", "site")).toBeNull();
  });

  test("a restarted node gets its in-flight jobs back at once", async () => {
    const { s, node } = await setup();
    await s.upsertPush("u1", { target: "hosting", name: "site", node, files: FILES });
    expect(await s.claimJobs("u1", node.id)).toHaveLength(1);
    expect(await s.claimJobs("u1", node.id)).toHaveLength(0);
    await s.registerNode("u1", { name: "seed2", payments: "coinpay" });
    const [again] = await s.claimJobs("u1", node.id);
    expect(again?.name).toBe("site");
  });

  test("a node that registers again keeps its id", async () => {
    const { s, node } = await setup();
    const again = await s.registerNode("u1", { name: "seed2", payments: "off" });
    expect(again.id).toBe(node.id);
    expect(again.payments).toBe("off");
  });
});
