// Fetching submitted sites: does the URL answer 200, does the page (or its
// homepage) carry the c0mpute gallery badge, and is the site safe to list.
//
// Every fetch goes through safeFetch: http/https on the default ports only,
// each hop's host resolved and refused if any address is private, loopback,
// link-local or otherwise not on the public internet, redirects followed by
// hand (at most 5), 10 s per hop, and at most 1.5 MB of body read.

import { lookup } from "node:dns/promises";
import { isIP } from "node:net";
import { checkPolicy } from "../safety/content-policy";
import { homepageOf } from "./url";

export const VERIFIER_UA = "c0mpute-gallery-verifier/1.0 (+https://c0mpute.com/gallery/submit)";
const MAX_BODY = 1_500_000;
const MAX_HOPS = 5;
const HOP_TIMEOUT_MS = 10_000;

export class FetchRefused extends Error {}

/** True for any address a server-side fetch must never reach. */
export function isPrivateAddress(ip: string): boolean {
  const v = isIP(ip);
  if (v === 4) return privateV4(ip);
  if (v === 6) {
    const a = ip.toLowerCase().split("%")[0];
    const mapped = /^(?:0{0,4}:){0,5}(?:0{0,4}:)?ffff:(\d{1,3}(?:\.\d{1,3}){3})$/.exec(a) ?? /^::ffff:(\d{1,3}(?:\.\d{1,3}){3})$/.exec(a);
    if (mapped) return privateV4(mapped[1]);
    if (a === "::" || a === "::1") return true;
    const first = parseInt(a.split(":")[0] || "0", 16);
    if ((first & 0xfe00) === 0xfc00) return true; // fc00::/7 unique local
    if ((first & 0xffc0) === 0xfe80) return true; // fe80::/10 link-local
    if ((first & 0xff00) === 0xff00) return true; // ff00::/8 multicast
    if (a.startsWith("64:ff9b:") || a.startsWith("2001:db8:") || a.startsWith("100::")) return true;
    if (a.startsWith("2002:")) return true; // 6to4 can wrap a private v4
    return false;
  }
  return true;
}

function privateV4(ip: string): boolean {
  const [a, b] = ip.split(".").map(Number);
  return (
    a === 0 ||
    a === 10 ||
    a === 127 ||
    (a === 100 && b >= 64 && b <= 127) ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 0) ||
    (a === 192 && b === 168) ||
    (a === 198 && (b === 18 || b === 19)) ||
    a >= 224
  );
}

export type Resolver = (host: string) => Promise<string[]>;
const dnsResolver: Resolver = async (host) => (await lookup(host, { all: true, verbatim: true })).map((r) => r.address);

async function assertPublicUrl(u: URL, resolve: Resolver): Promise<void> {
  if (u.protocol !== "https:" && u.protocol !== "http:") throw new FetchRefused(`refused scheme ${u.protocol}`);
  if (u.username || u.password) throw new FetchRefused("refused credentials in URL");
  if (u.port && !(u.port === "443" || u.port === "80")) throw new FetchRefused(`refused port ${u.port}`);
  const host = u.hostname.replace(/^\[|\]$/g, "");
  const addrs = isIP(host) ? [host] : await resolve(host).catch(() => []);
  if (!addrs.length) throw new FetchRefused(`${host} does not resolve`);
  if (addrs.some(isPrivateAddress)) throw new FetchRefused(`${host} resolves to a private address`);
}

export type Fetched = { status: number; finalUrl: string; html: string; contentType: string };

export type SafeFetchDeps = { fetch?: typeof fetch; resolve?: Resolver };

/** GET a public URL with SSRF guards; the body is read as text up to 1.5 MB. */
export async function safeFetch(url: string, deps: SafeFetchDeps = {}): Promise<Fetched> {
  const doFetch = deps.fetch ?? fetch;
  const resolve = deps.resolve ?? dnsResolver;
  let current = new URL(url);
  for (let hop = 0; hop <= MAX_HOPS; hop++) {
    await assertPublicUrl(current, resolve);
    const res = await doFetch(current.toString(), {
      redirect: "manual",
      headers: { "user-agent": VERIFIER_UA, accept: "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5" },
      signal: AbortSignal.timeout(HOP_TIMEOUT_MS),
    });
    if (res.status >= 300 && res.status < 400 && res.headers.get("location")) {
      await res.body?.cancel().catch(() => {});
      current = new URL(res.headers.get("location")!, current);
      continue;
    }
    const contentType = res.headers.get("content-type") ?? "";
    const html = /html|xml|text/i.test(contentType) || !contentType ? await readCapped(res) : (await res.body?.cancel().catch(() => {}), "");
    return { status: res.status, finalUrl: current.toString(), html, contentType };
  }
  throw new FetchRefused("too many redirects");
}

async function readCapped(res: Response): Promise<string> {
  if (!res.body) return "";
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let n = 0;
  while (n < MAX_BODY) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    n += value.byteLength;
  }
  await reader.cancel().catch(() => {});
  const all = new Uint8Array(Math.min(n, MAX_BODY));
  let o = 0;
  for (const c of chunks) {
    const take = Math.min(c.byteLength, all.length - o);
    all.set(c.subarray(0, take), o);
    o += take;
    if (o >= all.length) break;
  }
  return new TextDecoder().decode(all);
}

// An <a href>, <img src> (or srcset, <source>) pointing at the badge or the gallery.
const BADGE_RE =
  /<(?:a|img|source|link)\b[^>]*?\b(?:href|src|srcset)\s*=\s*["']?\s*(?:https?:)?\/\/(?:www\.)?c0mpute\.com\/(?:badge\.svg|gallery)(?=[\s"'?#/>&]|$)/i;

/** Does this HTML carry a link or image pointing at c0mpute.com/badge.svg or c0mpute.com/gallery? */
export function hasBadge(html: string): boolean {
  return BADGE_RE.test(html);
}

export type BadgeResult =
  /** found: badge present; missing: pages fetched fine and no badge; error: could not tell (site down, refused). */
  { state: "found" | "missing" | "error"; checked: string[]; note: string };

/** Look for the badge on the listed URL, then on its homepage. */
export async function checkBadge(url: string, deps: SafeFetchDeps = {}, first?: Fetched): Promise<BadgeResult> {
  const pages = [url, homepageOf(url)].filter((p, i, a) => a.indexOf(p) === i);
  const checked: string[] = [];
  let errors = 0;
  let lastError = "";
  for (const p of pages) {
    try {
      const f = p === url && first ? first : await safeFetch(p, deps);
      checked.push(p);
      if (f.status !== 200) {
        errors++;
        lastError = `${p} answered ${f.status}`;
        continue;
      }
      if (hasBadge(f.html)) return { state: "found", checked, note: `badge found on ${p}` };
    } catch (e) {
      errors++;
      lastError = `${p}: ${e instanceof Error ? e.message : String(e)}`;
    }
  }
  if (errors === pages.length) return { state: "error", checked, note: lastError || "could not fetch the site" };
  return { state: "missing", checked, note: `no c0mpute badge on ${checked.join(" or ")}` };
}

export type Reputation = { listed: boolean; reasons: string[]; error?: string };
export type ReputationLookup = (host: string) => Promise<Reputation>;

/** NicheDB's keyless domain reputation (Spamhaus DBL, URIBL, SURBL, ThreatCrush indicators). */
export const nichedbReputation: ReputationLookup = async (host) => {
  try {
    const res = await fetch(`https://nichedb.dev/api/v1/reputation/${encodeURIComponent(host)}`, {
      headers: { accept: "application/json", "user-agent": VERIFIER_UA },
      signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) return { listed: false, reasons: [], error: `reputation lookup HTTP ${res.status}` };
    const b = (await res.json()) as {
      listed?: boolean;
      reasons?: unknown[];
      dnsbl?: { list?: string; status?: string }[];
      indicators?: unknown[];
    };
    const lists = (b.dnsbl ?? []).filter((d) => d.status && d.status !== "clear").map((d) => `${d.list}: ${d.status}`);
    const reasons = [...(b.reasons ?? []).map(String), ...lists];
    return { listed: b.listed === true || lists.length > 0, reasons };
  } catch (e) {
    return { listed: false, reasons: [], error: `reputation lookup failed: ${e instanceof Error ? e.message : e}` };
  }
};

const ADULT_LABEL = /RTA-5042-1996-1400-1577-RTA|<meta[^>]+name\s*=\s*["']?rating["']?[^>]+content\s*=\s*["']?(adult|mature|RTA-5042)/i;

function pageText(html: string): string {
  const title = /<title[^>]*>([^<]{0,300})/i.exec(html)?.[1] ?? "";
  const desc = /<meta[^>]+name\s*=\s*["']?description["']?[^>]+content\s*=\s*["']([^"']{0,500})/i.exec(html)?.[1] ?? "";
  return `${title}\n${desc}`;
}

export type Checks = {
  /** Every automated check passed: it may go live without a person. */
  ok: boolean;
  /** Refuse outright (not 200, blocklisted, child-safety policy). */
  refuse: boolean;
  /** Needs a person (adult label, explicit wording). */
  hold: boolean;
  status: number | null;
  notes: string[];
  page?: Fetched;
};

/**
 * The automated checks every listing passes before it goes live: the URL
 * answers 200, the domain is not on a blocklist, and neither the listing text
 * nor the page's title/description trips the content policy or carries an
 * adult rating label.
 */
export async function runChecks(
  url: string,
  text: { name?: string | null; tagline?: string | null },
  deps: SafeFetchDeps & { reputation?: ReputationLookup } = {},
): Promise<Checks> {
  const notes: string[] = [];
  let page: Fetched | undefined;
  try {
    page = await safeFetch(url, deps);
  } catch (e) {
    return { ok: false, refuse: true, hold: false, status: null, notes: [`could not fetch ${url}: ${e instanceof Error ? e.message : e}`] };
  }
  if (page.status !== 200) {
    return { ok: false, refuse: true, hold: false, status: page.status, notes: [`${url} answered ${page.status}, not 200`], page };
  }
  const host = new URL(url).hostname.replace(/^www\./, "");
  const rep = await (deps.reputation ?? nichedbReputation)(host);
  if (rep.error) notes.push(rep.error);
  if (rep.listed) {
    return { ok: false, refuse: true, hold: false, status: 200, notes: [...notes, `domain is blocklisted (${rep.reasons.join("; ") || "listed"})`], page };
  }
  let hold = false;
  const policy = checkPolicy({ prompt: [text.name ?? "", text.tagline ?? "", pageText(page.html)].join("\n") });
  if (policy === "minor_sexual") {
    return { ok: false, refuse: true, hold: false, status: 200, notes: [...notes, "refused by the content policy"], page };
  }
  if (policy) {
    hold = true;
    notes.push("explicit wording: needs a person to review");
  }
  if (ADULT_LABEL.test(page.html)) {
    hold = true;
    notes.push("adult rating label on the page: needs a person to review");
  }
  return { ok: !hold, refuse: false, hold, status: 200, notes, page };
}
