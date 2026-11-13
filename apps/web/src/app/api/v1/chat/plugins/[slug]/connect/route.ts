import { connectLink, consumeTicket, pluginErr, startConnect, ticketForOtherAccount, verifyTicket, STATE_COOKIE, CALLBACK_PATH } from "@/lib/chat/plugins";
import { bearer, currentUser, json, ownOrigin, pluginStore, store } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/chat/plugins/<slug>/connect: sign in to a plugin.
 *
 * In a browser with the c0mpute session cookie this registers c0mpute with the
 * plugin's authorization server if needed, then redirects there; the person
 * signs in and lands on /api/v1/chat/plugins/callback. With an API key (no
 * browser) it answers JSON with a fresh one-time link to open.
 *
 * The one-time `?t=` link the API and CLI hand out names the account that will
 * receive the plugin's tokens, so it never redirects straight away: it shows
 * which account and which plugin, and only the button on that page (a POST from
 * our own origin, which uses the ticket up) goes on to the provider. Without
 * that, a link made on one account and opened by someone else would hand the
 * first account the second person's sign-in at the provider.
 */
export async function GET(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const url = new URL(req.url);
  const ticket = url.searchParams.get("t");
  if (ticket) {
    const ticketUser = verifyTicket(ticket, slug);
    if (!ticketUser) return page("Cannot connect the plugin", "This sign-in link has expired. Run the add (or /plugin connect) again for a fresh one.", 400);
    const refused = await refuseOtherAccount(req, ticketUser);
    if (refused) return refused;
    return confirmPage(ticketUser, slug, ticket);
  }
  const user = await currentUser(req);
  if (!user) {
    const next = `${url.pathname}${url.search}`;
    return new Response(null, { status: 302, headers: { location: `/api/auth/coinpay/login?next=${encodeURIComponent(next)}`, "cache-control": "no-store" } });
  }
  if (bearer(req)) return json({ connect_url: connectLink(user.id, slug) });
  return go(user.id, slug, 302);
}

/** The confirm button of the ticket page: uses the ticket up, then goes to the provider. */
export async function POST(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  // Only our own page may press the button: a form on another site must not
  // be able to skip the confirmation.
  if (!ownOrigin(req)) return page("Cannot connect the plugin", "Open the sign-in link again and confirm there.", 403);
  const form = await req.formData().catch(() => null);
  const ticket = form?.get("t");
  if (typeof ticket !== "string" || !ticket) return page("Cannot connect the plugin", "This sign-in link is not valid.", 400);
  const ticketUser = verifyTicket(ticket, slug);
  if (!ticketUser) return page("Cannot connect the plugin", "This sign-in link has expired. Run the add (or /plugin connect) again for a fresh one.", 400);
  const refused = await refuseOtherAccount(req, ticketUser);
  if (refused) return refused;
  let userId: string | null;
  try {
    userId = await consumeTicket(ticket, slug, pluginStore());
  } catch (e) {
    console.error("plugin connect: ticket claim failed", e);
    return page("Cannot connect the plugin", "Could not start the sign-in right now. Try again in a moment.", 503);
  }
  if (!userId) return page("Cannot connect the plugin", "This sign-in link was already used. Run the add (or /plugin connect) again for a fresh one.", 400);
  // 303: the browser follows with a GET to the provider.
  return go(userId, slug, 303);
}

async function refuseOtherAccount(req: Request, ticketUser: string): Promise<Response | null> {
  const here = await currentUser(req);
  if (!ticketForOtherAccount(ticketUser, here?.id)) return null;
  return page(
    "Cannot connect the plugin",
    "This sign-in link belongs to a different c0mpute account than the one signed in here. Start the connection from your own chat instead.",
    403,
  );
}

async function go(userId: string, slug: string, status: 302 | 303) {
  try {
    const { authorizeUrl, state } = await startConnect(userId, slug);
    return new Response(null, {
      status,
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
    return page("Cannot connect the plugin", body.error?.message ?? "could not start the sign-in", r.status);
  }
}

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

const HEAD = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Plugin sign-in · c0mpute</title><body style="font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 16px">`;

// same-origin, not no-referrer: a form posted from a no-referrer page carries
// `Origin: null`, which the POST above would refuse.
const PAGE_HEADERS = { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "x-frame-options": "DENY", "referrer-policy": "same-origin" };

/** Which plugin, into which c0mpute account: the person confirms before any redirect. */
async function confirmPage(ticketUser: string, slug: string, ticket: string) {
  const [account, plugin] = await Promise.all([
    store().userById(ticketUser).catch(() => null),
    pluginStore().get(ticketUser, slug).catch(() => null),
  ]);
  if (!account || !plugin) return page("Cannot connect the plugin", `No chat plugin ${slug} on this account. Add it again for a fresh link.`, 404);
  const who = account.name ? `${account.name} (${account.did})` : account.did;
  const action = `/api/v1/chat/plugins/${encodeURIComponent(slug)}/connect`;
  return new Response(
    `${HEAD}<h1>Connect ${esc(plugin.title || plugin.name)}?</h1>` +
      `<p>This signs you in to <strong>${esc(plugin.title || plugin.name)}</strong> (${esc(new URL(plugin.url).host)}) and gives its tools to this c0mpute account:</p>` +
      `<p><strong>${esc(who)}</strong></p>` +
      `<p>Only continue if that is your account and you started this from your own c0mpute chat, CLI or app. If someone sent you this link, close this page.</p>` +
      `<form method="post" action="${esc(action)}"><input type="hidden" name="t" value="${esc(ticket)}"><button type="submit" style="font:inherit;padding:.5rem 1rem">Continue to sign in</button></form>` +
      `<p><a href="/chat">back to chat</a></p></body>`,
    { status: 200, headers: PAGE_HEADERS },
  );
}

function page(title: string, msg: string, status: number) {
  return new Response(`${HEAD}<h1>${esc(title)}</h1><p>${esc(msg)}</p><p><a href="/chat">back to chat</a></p></body>`, { status, headers: PAGE_HEADERS });
}
