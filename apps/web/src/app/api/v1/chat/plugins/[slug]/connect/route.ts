import { connectLink, pluginErr, startConnect, verifyTicket, STATE_COOKIE, CALLBACK_PATH } from "@/lib/chat/plugins";
import { bearer, currentUser, json } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/chat/plugins/<slug>/connect: sign in to a plugin.
 *
 * In a browser (the c0mpute session cookie, or the one-time `?t=` link the API
 * and CLI hand out) this registers c0mpute with the plugin's authorization
 * server if needed, then redirects there; the person signs in and lands on
 * /api/v1/chat/plugins/callback. With an API key (no browser) it answers JSON
 * with a fresh one-time link to open.
 */
export async function GET(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const url = new URL(req.url);
  const ticket = url.searchParams.get("t");
  let userId: string | null = ticket ? verifyTicket(ticket, slug) : null;
  if (ticket && !userId) return page("This sign-in link has expired. Run the add (or /plugin connect) again for a fresh one.", 400);
  if (!userId) {
    const user = await currentUser(req);
    if (!user) {
      const next = `${url.pathname}${url.search}`;
      return new Response(null, { status: 302, headers: { location: `/api/auth/coinpay/login?next=${encodeURIComponent(next)}`, "cache-control": "no-store" } });
    }
    if (bearer(req)) return json({ connect_url: connectLink(user.id, slug) });
    userId = user.id;
  }
  try {
    const { authorizeUrl, state } = await startConnect(userId, slug);
    return new Response(null, {
      status: 302,
      headers: {
        location: authorizeUrl,
        // Binds the callback to this browser: the state in the URL must match.
        "set-cookie": `${STATE_COOKIE}=${state}; Path=${CALLBACK_PATH}; HttpOnly; Secure; SameSite=Lax; Max-Age=900`,
        "cache-control": "no-store",
        "referrer-policy": "no-referrer",
      },
    });
  } catch (e) {
    const r = pluginErr(e);
    const body = (await r.json().catch(() => ({}))) as { error?: { message?: string } };
    return page(body.error?.message ?? "could not start the sign-in", r.status);
  }
}

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

function page(msg: string, status: number) {
  return new Response(
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Plugin sign-in · c0mpute</title><body style="font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 16px"><h1>Cannot connect the plugin</h1><p>${esc(msg)}</p><p><a href="/chat">back to chat</a></p></body>`,
    { status, headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "x-frame-options": "DENY" } },
  );
}
