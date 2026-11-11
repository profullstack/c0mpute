import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { randomBytes } from "node:crypto";
import type { Sql } from "./store";
import { createPluginStore, pluginKey, publicPlugin, seal, slugify, unseal, PluginError } from "./plugin-store";
import { checkPluginUrl, isBlockedAddress, PluginUrlError } from "./plugin-net";
import { parsePluginCommand } from "./plugin-shared";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}
const key = randomBytes(32);
const AUTH = { issuer: "https://auth.example.com", authorization_endpoint: "https://auth.example.com/authorize", token_endpoint: "https://auth.example.com/token" };

describe("sealing", () => {
  test("round trip, bound to its row", () => {
    const s = seal(key, "secret-token", "u1|tc|token");
    expect(s.startsWith("v1.")).toBe(true);
    expect(s).not.toContain("secret-token");
    expect(unseal(key, s, "u1|tc|token")).toBe("secret-token");
    expect(unseal(key, s, "u2|tc|token")).toBeNull();
    expect(unseal(randomBytes(32), s, "u1|tc|token")).toBeNull();
  });
  test("key formats", () => {
    expect(pluginKey(key.toString("hex"))?.equals(key)).toBe(true);
    expect(pluginKey(key.toString("base64"))?.equals(key)).toBe(true);
    expect(pluginKey("short")).toBeNull();
    expect(pluginKey("")).toBeNull();
  });
});

describe("plugin store", () => {
  test("add, slug, list per account, toggle, remove", async () => {
    const s = createPluginStore(memSql(), { key });
    const a = await s.add("u1", { url: "https://tc.example.com/mcp", name: "ThreatCrush", auth: AUTH });
    expect(a.slug).toBe("threatcrush");
    expect(a.enabled).toBe(true);
    const again = await s.add("u1", { url: "https://tc.example.com/mcp", name: "ThreatCrush v2", auth: AUTH });
    expect(again.slug).toBe("threatcrush");
    const b = await s.add("u1", { url: "https://other.example.com/mcp", name: "ThreatCrush", auth: "none" });
    expect(b.slug).toBe("threatcrush-2");
    expect(await s.list("u1")).toHaveLength(2);
    expect(await s.list("u2")).toHaveLength(0);
    expect(await s.get("u2", "threatcrush")).toBeNull();
    expect((await s.setEnabled("u1", "threatcrush", false))?.enabled).toBe(false);
    expect(await s.remove("u2", "threatcrush")).toBe(false);
    expect(await s.remove("u1", "threatcrush")).toBe(true);
    expect(await s.list("u1")).toHaveLength(1);
  });

  test("reserved slug never shadows the callback route", async () => {
    const s = createPluginStore(memSql(), { key });
    expect((await s.add("u1", { url: "https://x.example.com/mcp", name: "callback", auth: "none" })).slug).not.toBe("callback");
  });

  test("tokens are sealed at rest and never public", async () => {
    const sql = memSql();
    const s = createPluginStore(sql, { key });
    await s.add("u1", { url: "https://tc.example.com/mcp", name: "tc", auth: AUTH });
    await s.setTokens("u1", "tc", { access_token: "at-123", refresh_token: "rt-456", expires_at: 1, client_id: "c1", token_endpoint: "https://auth.example.com/token" });
    const raw = (await sql.all("SELECT tokens FROM chat_plugins"))[0];
    expect(String(raw.tokens)).not.toContain("at-123");
    expect(String(raw.tokens)).not.toContain("rt-456");
    const p = (await s.get("u1", "tc"))!;
    expect(p.token).toBe("at-123");
    expect(p.tokens?.refresh_token).toBe("rt-456");
    expect(p.tokens?.client_id).toBe("c1");
    const pub = publicPlugin(p, "https://c0mpute.com");
    expect(JSON.stringify(pub)).not.toContain("at-123");
    expect(pub.status).toBe("on");
    await s.clearTokens("u1", "tc");
    expect(publicPlugin((await s.get("u1", "tc"))!, "https://c0mpute.com").status).toBe("needs_sign_in");
  });

  test("no key: sign-in refuses, authless still works", async () => {
    const s = createPluginStore(memSql(), { key: null });
    await s.add("u1", { url: "https://a.example.com/mcp", name: "a", auth: "none" });
    await expect(s.setTokens("u1", "a", { access_token: "x" })).rejects.toBeInstanceOf(PluginError);
  });

  test("pending sign-ins are one-shot and bound to the account", async () => {
    const s = createPluginStore(memSql(), { key });
    await s.savePending("u1", "tc", "state-abc", { verifier: "v", state: "state-abc" });
    const got = await s.takePending("state-abc");
    expect(got).toEqual({ user_id: "u1", slug: "tc", pending: { verifier: "v", state: "state-abc" } });
    expect(await s.takePending("state-abc")).toBeNull();
    expect(await s.takePending("nope")).toBeNull();
  });

  test("slugify", () => {
    expect(slugify("Thr3at Crush!!")).toBe("thr3at-crush");
    expect(slugify("???")).toBe("plugin");
    expect(slugify("x".repeat(80)).length).toBeLessThanOrEqual(32);
  });
});

describe("network guard", () => {
  test("blocked addresses", () => {
    for (const ip of ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "::", "fe80::1", "fd00::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254", "64:ff9b::a9fe:a9fe", "224.0.0.1"]) {
      expect(isBlockedAddress(ip)).toBe(true);
    }
    for (const ip of ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111", "::ffff:8.8.8.8"]) expect(isBlockedAddress(ip)).toBe(false);
  });

  test("url shape", () => {
    expect(() => checkPluginUrl("http://example.com/mcp")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://user:pw@example.com/mcp")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://localhost/mcp")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://127.0.0.1/mcp")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://[::1]/mcp")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://metadata.google.internal/")).toThrow(PluginUrlError);
    expect(() => checkPluginUrl("https://intranet/mcp")).toThrow(PluginUrlError);
    expect(checkPluginUrl("https://example.com/mcp#x").href).toBe("https://example.com/mcp");
  });

  test("optional sign-in counts as connected", async () => {
    const s = createPluginStore(memSql(), { key });
    const p = await s.add("u1", { url: "https://o.example.com/mcp", name: "o", auth: { ...AUTH, required: false } });
    const pub = publicPlugin(p, "https://c0mpute.com");
    expect(pub).toMatchObject({ auth: "optional", connected: true, signed_in: false, status: "on" });
    expect(pub.connect_url).toBe("https://c0mpute.com/api/v1/chat/plugins/o/connect");
  });
});

describe("/plugin command", () => {
  test("parses", () => {
    expect(parsePluginCommand("/plugin add https://x.example.com/mcp")).toEqual({ kind: "add", target: "https://x.example.com/mcp" });
    expect(parsePluginCommand("/plugin add threatcrush")).toEqual({ kind: "add", target: "threatcrush" });
    expect(parsePluginCommand("/plugin https://x.example.com/mcp")).toEqual({ kind: "add", target: "https://x.example.com/mcp" });
    expect(parsePluginCommand("/plugin ls")).toEqual({ kind: "ls" });
    expect(parsePluginCommand("/plugins")).toEqual({ kind: "ls" });
    expect(parsePluginCommand("/plugin rm threatcrush")).toEqual({ kind: "rm", slug: "threatcrush" });
    expect(parsePluginCommand("/plugin off tc")).toEqual({ kind: "off", slug: "tc" });
    expect(parsePluginCommand("/plugin")).toEqual({ kind: "help" });
    expect(parsePluginCommand("/plugin rm")?.kind).toBe("help");
    expect(parsePluginCommand("/pluginx")).toBeNull();
    expect(parsePluginCommand("plugin add x")).toBeNull();
  });
});
