// Server-side half of the promo request check. promo.ts is also bundled into
// the chat page, so the address rules (which need node:net) live here.

import { isPublicHostname } from "./plugin-net";
import { parsePromoRequest } from "./promo";

/**
 * parsePromoRequest, plus: the site must be a public host. The network fetches
 * that page to read the brand, so it must never be a loopback, private,
 * link-local, CGNAT or metadata address, nor an internal-looking name
 * (localhost, *.local, *.internal, a trailing-dot spelling of one).
 */
export function parsePublicPromoRequest(body: unknown): ReturnType<typeof parsePromoRequest> {
  const parsed = parsePromoRequest(body);
  if (typeof parsed === "string") return parsed;
  const host = new URL(parsed.url).hostname;
  if (host.endsWith(".") || !isPublicHostname(host)) return "url must name a public host";
  return parsed;
}
