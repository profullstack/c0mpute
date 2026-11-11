import { finishConnect, STATE_COOKIE, CALLBACK_PATH } from "@/lib/chat/plugins";
import { siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

function cookie(req: Request, name: string): string | null {
  for (const part of (req.headers.get("cookie") ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && part.slice(0, i).trim() === name) return decodeURIComponent(part.slice(i + 1).trim());
  }
  return null;
}

/**
 * Where a plugin's authorization server sends the person back. The state must
 * be one we issued (stored server-side for the account that started it) AND
 * match the cookie set on this browser at /connect, so a code cannot be
 * planted into someone else's account. The page tells the chat tab (popup
 * opener) it worked and closes.
 */
export async function GET(req: Request) {
  const url = new URL(req.url);
  const state = url.searchParams.get("state") ?? "";
  const code = url.searchParams.get("code") ?? "";
  const err = url.searchParams.get("error");
  const clear = `${STATE_COOKIE}=; Path=${CALLBACK_PATH}; HttpOnly; Secure; SameSite=Lax; Max-Age=0`;
  if (!state || cookie(req, STATE_COOKIE) !== state) return page("Sign-in failed", "This sign-in did not start in this browser, or it expired. Start it again from the chat.", null, clear, 400);
  if (err) return page("Sign-in cancelled", url.searchParams.get("error_description") || err, null, clear, 400);
  if (!code) return page("Sign-in failed", "No authorization code came back.", null, clear, 400);
  try {
    const { slug, name } = await finishConnect(state, code);
    return page(`${name} connected`, `Its tools are now available in c0mpute chat. You can close this window.`, slug, clear, 200);
  } catch (e) {
    return page("Sign-in failed", e instanceof Error ? e.message : String(e), null, clear, 400);
  }
}

function page(title: string, msg: string, slug: string | null, setCookie: string, status: number) {
  const origin = siteUrl();
  const script = slug
    ? `<script>try{if(window.opener){window.opener.postMessage({type:"c0-plugin-connected",slug:${JSON.stringify(slug)}},${JSON.stringify(origin)});setTimeout(function(){window.close()},800)}}catch(e){}</script>`
    : "";
  return new Response(
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${esc(title)} · c0mpute</title><body style="font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 16px"><h1>${esc(title)}</h1><p>${esc(msg)}</p><p><a href="/chat">open chat</a></p>${script}</body>`,
    {
      status,
      headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "set-cookie": setCookie, "x-frame-options": "DENY", "referrer-policy": "no-referrer" },
    },
  );
}
