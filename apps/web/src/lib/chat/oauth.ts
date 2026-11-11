// OAuth 2.1 for the remote MCP server (/api/mcp), so ChatGPT apps, claude.ai
// connectors and Claude Code can sign a person in without a pasted key.
//
// c0mpute.com is its own authorization server: dynamic client registration
// (RFC 7591, public clients only), the authorization code flow with PKCE S256,
// and a consent page behind the normal "Sign in with CoinPay" session. The
// access token it hands out is an ordinary c0k_ API key labelled
// "<client> (OAuth)", so the MCP route needs no new auth path and the person
// can see and revoke it with their other keys.

import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import type { Sql } from "./store";

export const CLIENT_PREFIX = "c0c_";
export const CODE_PREFIX = "c0a_";
const CODE_MINUTES = 5;
const MAX_REDIRECTS = 10;
export const SCOPE = "mcp";

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS oauth_clients (
     client_id     TEXT PRIMARY KEY,
     name          TEXT NOT NULL,
     redirect_uris TEXT NOT NULL,
     created_at    TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS oauth_codes (
     code_hash    TEXT PRIMARY KEY,
     client_id    TEXT NOT NULL,
     user_id      TEXT NOT NULL,
     redirect_uri TEXT NOT NULL,
     challenge    TEXT NOT NULL,
     scope        TEXT,
     expires_at   TEXT NOT NULL
   )`,
];

export type OAuthClient = { client_id: string; name: string; redirect_uris: string[]; created_at: string };

const sha256hex = (s: string) => createHash("sha256").update(s).digest("hex");
const now = () => new Date().toISOString();

export class OAuthError extends Error {
  constructor(
    public code: string,
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

/**
 * Redirect URIs a client may register: https anywhere (the consent page names
 * the host, so a person sees where they are being sent), or http on loopback
 * for desktop and CLI clients (RFC 8252). No fragments, no credentials.
 */
export function validRedirectUri(raw: unknown): raw is string {
  if (typeof raw !== "string" || raw.length > 2000) return false;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return false;
  }
  if (u.hash || u.username || u.password) return false;
  if (u.protocol === "https:") return true;
  return u.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(u.hostname);
}

/** Loopback redirects match on everything but the port (RFC 8252 7.3); others exactly. */
export function redirectMatches(registered: string[], uri: string): boolean {
  if (registered.includes(uri)) return true;
  let u: URL;
  try {
    u = new URL(uri);
  } catch {
    return false;
  }
  if (u.protocol !== "http:") return false;
  return registered.some((r) => {
    try {
      const x = new URL(r);
      return x.protocol === "http:" && x.hostname === u.hostname && x.pathname === u.pathname && x.search === u.search;
    } catch {
      return false;
    }
  });
}

/** PKCE S256: BASE64URL(SHA256(verifier)) == challenge. */
export function pkceOk(verifier: unknown, challenge: string): boolean {
  if (typeof verifier !== "string" || !/^[A-Za-z0-9._~-]{43,128}$/.test(verifier)) return false;
  const a = Buffer.from(createHash("sha256").update(verifier).digest("base64url"));
  const b = Buffer.from(challenge);
  return a.length === b.length && timingSafeEqual(a, b);
}

export function createOAuthStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const store = {
    init,

    /** RFC 7591 registration. Public clients only: no secret is issued or accepted. */
    async registerClient(body: Record<string, unknown>): Promise<OAuthClient> {
      await init();
      const uris = body.redirect_uris;
      if (!Array.isArray(uris) || uris.length === 0 || uris.length > MAX_REDIRECTS || !uris.every(validRedirectUri)) {
        throw new OAuthError("invalid_redirect_uri", "redirect_uris: 1-10 https URLs (or http on localhost)");
      }
      const method = body.token_endpoint_auth_method;
      if (method !== undefined && method !== "none") {
        throw new OAuthError("invalid_client_metadata", "token_endpoint_auth_method must be none (public clients with PKCE)");
      }
      const grants = body.grant_types;
      if (grants !== undefined && (!Array.isArray(grants) || !grants.includes("authorization_code"))) {
        throw new OAuthError("invalid_client_metadata", "grant_types must include authorization_code");
      }
      const raw = typeof body.client_name === "string" ? body.client_name : "";
      const name = raw.replace(/[\u0000-\u001f\u007f<>]/g, "").trim().slice(0, 80) || new URL(uris[0] as string).host;
      const client: OAuthClient = {
        client_id: CLIENT_PREFIX + randomBytes(16).toString("base64url"),
        name,
        redirect_uris: uris as string[],
        created_at: now(),
      };
      await sql.run(`INSERT INTO oauth_clients (client_id, name, redirect_uris, created_at) VALUES (?, ?, ?, ?)`, [
        client.client_id,
        client.name,
        JSON.stringify(client.redirect_uris),
        client.created_at,
      ]);
      return client;
    },

    async client(clientId: unknown): Promise<OAuthClient | null> {
      if (typeof clientId !== "string" || !clientId.startsWith(CLIENT_PREFIX) || clientId.length > 64) return null;
      await init();
      const r = (await sql.all(`SELECT * FROM oauth_clients WHERE client_id = ?`, [clientId]))[0];
      if (!r) return null;
      return { client_id: String(r.client_id), name: String(r.name), redirect_uris: JSON.parse(String(r.redirect_uris)), created_at: String(r.created_at) };
    },

    async createCode(c: { client_id: string; user_id: string; redirect_uri: string; challenge: string; scope?: string | null }) {
      await init();
      await sql.run(`DELETE FROM oauth_codes WHERE expires_at < ?`, [now()]);
      const code = CODE_PREFIX + randomBytes(24).toString("base64url");
      await sql.run(
        `INSERT INTO oauth_codes (code_hash, client_id, user_id, redirect_uri, challenge, scope, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)`,
        [sha256hex(code), c.client_id, c.user_id, c.redirect_uri, c.challenge, c.scope ?? null, new Date(Date.now() + CODE_MINUTES * 60_000).toISOString()],
      );
      return code;
    },

    /**
     * Redeem a code once. It is deleted before any check, so a replayed or
     * mismatched code can never be tried twice.
     */
    async redeemCode(code: unknown, p: { client_id: unknown; redirect_uri: unknown; code_verifier: unknown }) {
      if (typeof code !== "string" || !code.startsWith(CODE_PREFIX)) throw new OAuthError("invalid_grant", "unknown code");
      await init();
      const h = sha256hex(code);
      const r = (await sql.all(`SELECT * FROM oauth_codes WHERE code_hash = ?`, [h]))[0];
      if (!r) throw new OAuthError("invalid_grant", "unknown or used code");
      await sql.run(`DELETE FROM oauth_codes WHERE code_hash = ?`, [h]);
      if (String(r.expires_at) < now()) throw new OAuthError("invalid_grant", "code expired");
      if (p.client_id !== r.client_id) throw new OAuthError("invalid_grant", "code was issued to another client");
      if (p.redirect_uri !== r.redirect_uri) throw new OAuthError("invalid_grant", "redirect_uri does not match the authorization request");
      if (!pkceOk(p.code_verifier, String(r.challenge))) throw new OAuthError("invalid_grant", "PKCE verification failed");
      return { user_id: String(r.user_id), client_id: String(r.client_id), scope: (r.scope as string | null) ?? SCOPE };
    },
  };
  return store;
}

export type OAuthStore = ReturnType<typeof createOAuthStore>;

/** CSRF token for the consent form: bound to the browser session and this exact request. */
export function consentToken(session: string, q: { client_id: string; redirect_uri: string; code_challenge: string; state: string }) {
  return sha256hex(`consent|${session}|${q.client_id}|${q.redirect_uri}|${q.code_challenge}|${q.state}`);
}

export function tokenEq(a: string, b: string) {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

/** Metadata documents (RFC 8414, RFC 9728). */
export function authServerMetadata(site: string) {
  return {
    issuer: site,
    authorization_endpoint: `${site}/oauth/authorize`,
    token_endpoint: `${site}/api/oauth/token`,
    registration_endpoint: `${site}/api/oauth/register`,
    response_types_supported: ["code"],
    grant_types_supported: ["authorization_code"],
    code_challenge_methods_supported: ["S256"],
    token_endpoint_auth_methods_supported: ["none"],
    scopes_supported: [SCOPE],
    service_documentation: `${site}/docs`,
  };
}

export function protectedResourceMetadata(site: string) {
  return {
    resource: `${site}/api/mcp`,
    authorization_servers: [site],
    bearer_methods_supported: ["header"],
    scopes_supported: [SCOPE],
    resource_name: "c0mpute",
    resource_documentation: `${site}/docs`,
  };
}

export const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "authorization, content-type, mcp-protocol-version",
};
