// Gallery listings are keyed by URL, never by a product id: one normalized
// form decides duplicates, badge hit counts and which listing a badge is for.

export type NormalizedUrl = {
  /** The URL as we store and link it: scheme + lowercase host + path, no query, fragment or trailing slash. */
  url: string;
  /** The dedupe key: host without "www." + path (no scheme). */
  key: string;
  /** The lowercase host as given (keeps "www."). */
  host: string;
};

const MAX_URL = 300;

/**
 * Parse and normalize a site URL a person typed. A bare domain gets https://.
 * Refused (null): other schemes, credentials, IP literals, non-default ports,
 * hosts without a dot, and anything longer than 300 characters.
 */
export function normalizeUrl(input: unknown): NormalizedUrl | null {
  if (typeof input !== "string") return null;
  let raw = input.trim();
  if (!raw || raw.length > MAX_URL) return null;
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(raw)) raw = `https://${raw}`;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return null;
  }
  if (u.protocol !== "https:" && u.protocol !== "http:") return null;
  if (u.username || u.password || u.port) return null;
  const host = u.hostname.toLowerCase().replace(/\.$/, "");
  if (!host.includes(".") || isIpLiteral(host)) return null;
  if (!/^[a-z0-9.-]+$/.test(host) || host.startsWith(".") || host.includes("..")) return null;
  const path = u.pathname.replace(/\/{2,}/g, "/").replace(/\/+$/, "");
  const url = `${u.protocol}//${host}${path}`;
  if (url.length > MAX_URL) return null;
  return { url, key: `${host.replace(/^www\./, "")}${path}`, host };
}

function isIpLiteral(host: string): boolean {
  return /^\d{1,3}(\.\d{1,3}){3}$/.test(host) || host.startsWith("[") || host.includes(":");
}

/** The site's homepage (origin), for pages that carry the badge site-wide. */
export function homepageOf(url: string): string {
  const u = new URL(url);
  return `${u.protocol}//${u.host}`;
}

/** A URL-safe slug from a name or host: lowercase a-z0-9 and dashes, at most 48 chars. */
export function slugify(s: string): string {
  const out = s
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48)
    .replace(/-+$/, "");
  return out || "listing";
}

/** A c0mpute hosting gateway URL (gateway.c0mpute.com/s/<id>/), which is its own c0mpute copy. */
export function isGatewayUrl(url: string): boolean {
  return /^https:\/\/gateway\.c0mpute\.com\/s\/[A-Za-z0-9]+/.test(url);
}

/** Display host + path, e.g. "nixamp.com/music". */
export function displayUrl(url: string): string {
  const u = new URL(url);
  return u.host + (u.pathname === "/" ? "" : u.pathname.replace(/\/$/, ""));
}
