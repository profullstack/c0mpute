import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createHash } from "node:crypto";
import type { Sql } from "./store";
import { authServerMetadata, consentToken, createOAuthStore, pkceOk, protectedResourceMetadata, redirectMatches, validRedirectUri } from "./oauth";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const verifier = "a".repeat(20) + "B".repeat(20) + "-._~1234";
const challenge = createHash("sha256").update(verifier).digest("base64url");
const CLAUDE = "https://claude.ai/api/mcp/auth_callback";
const CHATGPT = "https://chatgpt.com/connector_platform_oauth_redirect";

describe("redirect uris", () => {
  test("https anywhere, http only on loopback, no fragments", () => {
    expect(validRedirectUri(CLAUDE)).toBe(true);
    expect(validRedirectUri(CHATGPT)).toBe(true);
    expect(validRedirectUri("http://localhost:33418/callback")).toBe(true);
    expect(validRedirectUri("http://127.0.0.1/cb")).toBe(true);
    expect(validRedirectUri("http://evil.com/cb")).toBe(false);
    expect(validRedirectUri("https://x.com/cb#frag")).toBe(false);
    expect(validRedirectUri("javascript:alert(1)")).toBe(false);
  });
  test("loopback matches on any port, others exactly", () => {
    expect(redirectMatches(["http://localhost/callback"], "http://localhost:5555/callback")).toBe(true);
    expect(redirectMatches(["http://localhost/callback"], "http://localhost:5555/other")).toBe(false);
    expect(redirectMatches([CLAUDE], CLAUDE)).toBe(true);
    expect(redirectMatches([CLAUDE], CLAUDE + "x")).toBe(false);
  });
});

describe("pkce", () => {
  test("S256", () => {
    expect(pkceOk(verifier, challenge)).toBe(true);
    expect(pkceOk(verifier + "x", challenge)).toBe(false);
    expect(pkceOk("short", challenge)).toBe(false);
  });
});

describe("oauth store", () => {
  test("register, code, redeem once", async () => {
    const o = createOAuthStore(memSql());
    const c = await o.registerClient({ client_name: "Claude", redirect_uris: [CLAUDE] });
    expect(c.client_id.startsWith("c0c_")).toBe(true);
    expect((await o.client(c.client_id))?.redirect_uris).toEqual([CLAUDE]);
    const code = await o.createCode({ client_id: c.client_id, user_id: "u1", redirect_uri: CLAUDE, challenge });
    const r = await o.redeemCode(code, { client_id: c.client_id, redirect_uri: CLAUDE, code_verifier: verifier });
    expect(r.user_id).toBe("u1");
    await expect(o.redeemCode(code, { client_id: c.client_id, redirect_uri: CLAUDE, code_verifier: verifier })).rejects.toThrow(/unknown or used/);
  });

  test("a wrong verifier burns the code", async () => {
    const o = createOAuthStore(memSql());
    const c = await o.registerClient({ client_name: "ChatGPT", redirect_uris: [CHATGPT] });
    const code = await o.createCode({ client_id: c.client_id, user_id: "u1", redirect_uri: CHATGPT, challenge });
    await expect(o.redeemCode(code, { client_id: c.client_id, redirect_uri: CHATGPT, code_verifier: "x".repeat(50) })).rejects.toThrow(/PKCE/);
    await expect(o.redeemCode(code, { client_id: c.client_id, redirect_uri: CHATGPT, code_verifier: verifier })).rejects.toThrow(/unknown or used/);
  });

  test("rejects bad metadata", async () => {
    const o = createOAuthStore(memSql());
    await expect(o.registerClient({ redirect_uris: ["http://evil.com/cb"] })).rejects.toThrow(/redirect_uris/);
    await expect(o.registerClient({ redirect_uris: [CLAUDE], token_endpoint_auth_method: "client_secret_basic" })).rejects.toThrow(/none/);
    const named = await o.registerClient({ client_name: "<script>x</script>", redirect_uris: [CLAUDE] });
    expect(named.name).not.toContain("<");
  });

  test("consent token is bound to the session and request", () => {
    const q = { client_id: "c0c_a", redirect_uri: CLAUDE, code_challenge: challenge, state: "s" };
    expect(consentToken("c0s_1", q)).not.toBe(consentToken("c0s_2", q));
    expect(consentToken("c0s_1", q)).not.toBe(consentToken("c0s_1", { ...q, state: "t" }));
  });

  test("metadata points at this site", () => {
    expect(authServerMetadata("https://c0mpute.com").registration_endpoint).toBe("https://c0mpute.com/api/oauth/register");
    expect(protectedResourceMetadata("https://c0mpute.com").resource).toBe("https://c0mpute.com/api/mcp");
  });
});
