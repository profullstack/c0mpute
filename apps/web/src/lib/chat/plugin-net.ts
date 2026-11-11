// Network guard for chat plugins. A chat plugin is a remote MCP server that a
// person names by URL, and c0mpute.com then fetches it from inside dev2.
// @profullstack/chat-plugins already guards every request it makes (https
// only, DNS resolved and every address public, redirects re-checked per hop,
// a bearer token never follows a redirect to another origin, body caps). This
// adds c0mpute's own rules on top, through the package's `guard` hook:
// no credentials in the URL, no internal-looking hostnames (localhost, *.local,
// *.internal, metadata.google.internal, single-label names) and no private,
// loopback, link-local, CGNAT, multicast or reserved IP literals, IPv4-mapped
// and NAT64 forms included.

import { isIP } from "node:net";

export class PluginUrlError extends Error {}

const V4_BLOCKS: [string, number][] = [
  ["0.0.0.0", 8],
  ["10.0.0.0", 8],
  ["100.64.0.0", 10],
  ["127.0.0.0", 8],
  ["169.254.0.0", 16],
  ["172.16.0.0", 12],
  ["192.0.0.0", 24],
  ["192.0.2.0", 24],
  ["192.88.99.0", 24],
  ["192.168.0.0", 16],
  ["198.18.0.0", 15],
  ["198.51.100.0", 24],
  ["203.0.113.0", 24],
  ["224.0.0.0", 4],
  ["240.0.0.0", 4],
];

const v4num = (ip: string) => ip.split(".").reduce((n, o) => (n << 8) + Number(o), 0) >>> 0;

function v4Blocked(ip: string): boolean {
  const n = v4num(ip);
  return V4_BLOCKS.some(([base, bits]) => {
    const mask = bits === 0 ? 0 : (0xffffffff << (32 - bits)) >>> 0;
    return (n & mask) === (v4num(base) & mask);
  });
}

/** Expand an IPv6 address to 8 hextets (numbers). */
function v6parts(ip: string): number[] | null {
  let s = ip.toLowerCase().split("%")[0];
  // Trailing dotted IPv4 (::ffff:1.2.3.4).
  const dotted = /(\d+\.\d+\.\d+\.\d+)$/.exec(s);
  if (dotted) {
    const n = v4num(dotted[1]);
    s = s.slice(0, -dotted[1].length) + `${(n >>> 16).toString(16)}:${(n & 0xffff).toString(16)}`;
  }
  const halves = s.split("::");
  if (halves.length > 2) return null;
  const head = halves[0] ? halves[0].split(":") : [];
  const tail = halves.length === 2 && halves[1] ? halves[1].split(":") : [];
  const fill = halves.length === 2 ? 8 - head.length - tail.length : 0;
  const all = [...head, ...Array(Math.max(0, fill)).fill("0"), ...tail];
  if (all.length !== 8) return null;
  const nums = all.map((h) => parseInt(h, 16));
  return nums.some((x) => !Number.isFinite(x) || x < 0 || x > 0xffff) ? null : nums;
}

function v6Blocked(ip: string): boolean {
  const p = v6parts(ip);
  if (!p) return true;
  const allZeroUntil = (k: number) => p.slice(0, k).every((x) => x === 0);
  // :: and ::1
  if (allZeroUntil(7) && (p[7] === 0 || p[7] === 1)) return true;
  // IPv4-mapped ::ffff:a.b.c.d and IPv4-compatible ::a.b.c.d
  if (allZeroUntil(5) && (p[5] === 0xffff || p[5] === 0)) {
    return v4Blocked(`${p[6] >> 8}.${p[6] & 255}.${p[7] >> 8}.${p[7] & 255}`);
  }
  // NAT64 64:ff9b::/96 carries an IPv4 address too.
  if (p[0] === 0x64 && p[1] === 0xff9b && p.slice(2, 6).every((x) => x === 0)) {
    return v4Blocked(`${p[6] >> 8}.${p[6] & 255}.${p[7] >> 8}.${p[7] & 255}`);
  }
  if ((p[0] & 0xfe00) === 0xfc00) return true; // fc00::/7 unique local
  if ((p[0] & 0xffc0) === 0xfe80) return true; // fe80::/10 link-local
  if ((p[0] & 0xffc0) === 0xfec0) return true; // fec0::/10 site-local (deprecated)
  if ((p[0] & 0xff00) === 0xff00) return true; // ff00::/8 multicast
  if (p[0] === 0x2001 && p[1] === 0x0db8) return true; // documentation
  if (p[0] === 0x2002) return true; // 6to4 can embed private v4
  if (p[0] === 0x0100 && allZeroUntil(4)) return false;
  return false;
}

/** True for any address a plugin may not point us at. */
export function isBlockedAddress(ip: string): boolean {
  const kind = isIP(ip);
  if (kind === 4) return v4Blocked(ip);
  if (kind === 6) return v6Blocked(ip);
  return true;
}

const BLOCKED_HOSTS = /(^|\.)(localhost|local|internal|localdomain|home\.arpa|metadata\.google\.internal)$/i;

/** Parse and check the shape of a plugin URL (no DNS). Returns the normalized URL. */
export function checkPluginUrl(raw: unknown): URL {
  if (typeof raw !== "string" || !raw.trim() || raw.length > 2000) throw new PluginUrlError("a plugin URL is required");
  let u: URL;
  try {
    u = new URL(raw.trim());
  } catch {
    throw new PluginUrlError("not a URL");
  }
  if (u.protocol !== "https:") throw new PluginUrlError("plugin URLs must be https://");
  if (u.username || u.password) throw new PluginUrlError("no credentials in plugin URLs");
  u.hash = "";
  const host = u.hostname.replace(/^\[|\]$/g, "");
  if (BLOCKED_HOSTS.test(host) || !host.includes(".") && !isIP(host)) throw new PluginUrlError(`${host} is not a public host`);
  if (isIP(host) && isBlockedAddress(host)) throw new PluginUrlError(`${host} is not a public address`);
  return u;
}

/**
 * Extra check for @profullstack/chat-plugins' `guard` option: it runs on every
 * URL the package fetches (each redirect hop too), after the package's own
 * https + DNS + private-range check. Adds the hostname rules above.
 */
export function pluginGuard(u: URL): void {
  checkPluginUrl(u.href);
}
