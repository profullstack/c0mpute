import { consentToken, redirectMatches, SCOPE, tokenEq } from "@/lib/chat/oauth";
import { currentUser, oauthStore, sessionToken, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * OAuth 2.1 authorization endpoint for the MCP server. GET checks the request,
 * sends a signed-out person through "Sign in with CoinPay" and back, and shows
 * a consent page; POST (the consent form) issues a code or access_denied.
 */

type Q = { client_id: string; redirect_uri: string; code_challenge: string; state: string; scope: string };

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

function page(title: string, body: string, status = 200) {
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${esc(title)} · c0mpute</title><meta name="robots" content="noindex">
<style>
:root{--bg:#fff8ec;--fg:#1f2440;--muted:#5b6080;--card:#fff;--line:#e6dcc8;--ok:#2bb673;--okfg:#06210f}
@media (prefers-color-scheme:dark){:root{--bg:#0f1a33;--fg:#eef1ff;--muted:#a8b0d8;--card:#17244a;--line:#2a3a6a;--ok:#2bb673;--okfg:#06210f}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif;padding:16px}
main{max-width:440px;width:100%;background:var(--card);border:1px solid var(--line);border-radius:18px;padding:28px}
h1{font-size:1.35rem;margin:0 0 .5rem}p{margin:.5rem 0;color:var(--muted)}b{color:var(--fg)}ul{padding-left:1.2rem;margin:.75rem 0}li{margin:.25rem 0}
.row{display:flex;gap:10px;margin-top:1.25rem}button{flex:1;font:inherit;font-weight:700;padding:.75rem 1rem;border-radius:12px;border:1px solid var(--line);background:transparent;color:var(--fg);cursor:pointer}
button.ok{background:var(--ok);border-color:var(--ok);color:var(--okfg)}small{display:block;margin-top:1rem;color:var(--muted)}code{font-size:.9em}
</style></head><body><main>${body}</main></body></html>`;
  return new Response(html, {
    status,
    headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "x-frame-options": "DENY", "referrer-policy": "same-origin" },
  });
}

const errorPage = (msg: string) => page("Cannot connect", `<h1>Cannot connect</h1><p>${esc(msg)}</p>`, 400);

function back(redirectUri: string, params: Record<string, string>) {
  const u = new URL(redirectUri);
  for (const [k, v] of Object.entries(params)) if (v) u.searchParams.set(k, v);
  u.searchParams.set("iss", siteUrl());
  return new Response(null, { status: 302, headers: { location: u.toString(), "cache-control": "no-store" } });
}

async function check(get: (k: string) => string | null) {
  const oauth = oauthStore();
  const client = await oauth.client(get("client_id"));
  if (!client) return { error: errorPage("Unknown app. Remove the connector and add it again.") } as const;
  const redirect = get("redirect_uri") ?? (client.redirect_uris.length === 1 ? client.redirect_uris[0] : "");
  if (!redirect || !redirectMatches(client.redirect_uris, redirect)) return { error: errorPage("That return address is not registered for this app.") } as const;
  const state = get("state") ?? "";
  if (get("response_type") !== "code") return { error: back(redirect, { error: "unsupported_response_type", state }) } as const;
  const challenge = get("code_challenge") ?? "";
  if (get("code_challenge_method") !== "S256" || !/^[A-Za-z0-9_-]{43,128}$/.test(challenge)) {
    return { error: back(redirect, { error: "invalid_request", error_description: "PKCE S256 required", state }) } as const;
  }
  const q: Q = { client_id: client.client_id, redirect_uri: redirect, code_challenge: challenge, state, scope: SCOPE };
  return { client, q } as const;
}

export async function GET(req: Request) {
  const url = new URL(req.url);
  const r = await check((k) => url.searchParams.get(k));
  if ("error" in r) return r.error;
  const user = await currentUser(req);
  const session = sessionToken(req);
  if (!user || user.token_kind !== "session" || !session) {
    const next = `/oauth/authorize${url.search}`;
    return new Response(null, { status: 302, headers: { location: `${siteUrl()}/api/auth/coinpay/login?next=${encodeURIComponent(next)}`, "cache-control": "no-store" } });
  }
  const { client, q } = r;
  const host = new URL(q.redirect_uri).host;
  const csrf = consentToken(session, q);
  const hidden = Object.entries({ ...q, csrf })
    .map(([k, v]) => `<input type="hidden" name="${k}" value="${esc(v)}">`)
    .join("");
  return page(
    "Connect",
    `<h1>Connect <b>${esc(client.name)}</b> to c0mpute?</h1>
<p>Signed in as <b>${esc(user.name || user.did)}</b>.</p>
<p>${esc(client.name)} will be able to:</p>
<ul><li>publish, update and take down websites on the c0mpute network for you</li><li>ask models on the network, billed to your compute balance</li><li>make promo videos</li></ul>
<form method="post" action="/oauth/authorize">${hidden}
<div class="row"><button type="submit" name="decision" value="deny">Cancel</button><button class="ok" type="submit" name="decision" value="allow">Allow</button></div></form>
<small>You will be sent back to <code>${esc(host)}</code>. It gets an API key you can revoke any time in c0mpute chat (keys).</small>`,
  );
}

export async function POST(req: Request) {
  // The consent form is CSRF-bound to the session (consentToken below). The
  // origin check is extra: refuse a real foreign origin, but not `null`, which
  // browsers send for a form POST from a page with a strict referrer policy
  // (a cached copy of this page from before same-origin was set, privacy modes).
  const origin = req.headers.get("origin");
  if (origin && origin !== "null" && origin !== siteUrl() && origin !== new URL(req.url).origin) return errorPage("Cross-site request refused.");
  const form = await req.formData().catch(() => null);
  if (!form) return errorPage("Bad request.");
  const get = (k: string) => {
    const v = form.get(k);
    return typeof v === "string" ? v : null;
  };
  // check() wants the GET shape; the form carries the same fields.
  const r = await check((k) => (k === "response_type" ? "code" : k === "code_challenge_method" ? "S256" : get(k)));
  if ("error" in r) return r.error;
  const { q } = r;
  const user = await currentUser(req);
  const session = sessionToken(req);
  if (!user || user.token_kind !== "session" || !session) return errorPage("Your sign-in expired. Start the connection again from the app.");
  if (!tokenEq(get("csrf") ?? "", consentToken(session, q))) return errorPage("This consent page expired. Start the connection again from the app.");
  if (get("decision") !== "allow") return back(q.redirect_uri, { error: "access_denied", state: q.state });
  const code = await oauthStore().createCode({ client_id: q.client_id, user_id: user.id, redirect_uri: q.redirect_uri, challenge: q.code_challenge, scope: q.scope });
  return back(q.redirect_uri, { code, state: q.state });
}
