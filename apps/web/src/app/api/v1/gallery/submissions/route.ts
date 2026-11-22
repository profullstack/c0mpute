import { clientIp, rateLimit } from "@/lib/chat/server";
import { GALLERY_CONFIG } from "@/lib/gallery/config";
import { gerror, gjson, goptions, readBody } from "@/lib/gallery/http";
import { gallery } from "@/lib/gallery/server";
import { parseSubmission, publicStatus } from "@/lib/gallery/service";
import { normalizeUrl } from "@/lib/gallery/url";

export const dynamic = "force-dynamic";
export const maxDuration = 60;

/**
 * POST /api/v1/gallery/submissions: get listed. JSON or form fields
 * url (required), name, tagline, category, built_with, contact_email,
 * tier ("free" | "priority"). No account needed.
 *   201 { listing, checkout_url, message }   409 already submitted (with its status)   422 URL not 200 / not listable
 */
export async function POST(req: Request) {
  const rl = rateLimit(`gallery-submit:${clientIp(req)}`, GALLERY_CONFIG.submitsPerHour);
  if (!rl.ok) return gjson({ error: { message: "too many submissions from here this hour", code: 429 } }, 429, { "retry-after": String(rl.retryAfter) });
  const body = await readBody(req);
  if (!body) return gjson({ error: { message: "send JSON or form fields", code: 400 } }, 400);
  const input = parseSubmission(body);
  if (typeof input === "string") return gjson({ error: { message: input, code: 400 } }, 400);
  try {
    const r = await gallery().submit(input);
    return gjson({ listing: publicStatus(r.listing), checkout_url: r.checkout_url, message: r.message }, 201);
  } catch (e) {
    return gerror(e);
  }
}

/** GET /api/v1/gallery/submissions?url=<site url>: a listing's public status by its URL. */
export async function GET(req: Request) {
  const norm = normalizeUrl(new URL(req.url).searchParams.get("url"));
  if (!norm) return gjson({ error: { message: "pass ?url=<your site url>", code: 400 } }, 400);
  try {
    const l = await gallery().store.byKey(norm.key);
    if (!l) return gjson({ error: { message: "not submitted yet: https://c0mpute.com/gallery/submit", code: 404 } }, 404);
    return gjson({ listing: publicStatus(l, await gallery().store.hits(l.url_key)) });
  } catch (e) {
    return gerror(e);
  }
}

export const OPTIONS = goptions;
