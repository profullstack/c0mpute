// Anonymity for the public leaderboard (/leaderboard, /api/leaderboard).
//
// Pure, so the rules are tested in one place:
//  - A person is a token: HMAC-SHA256 keyed with our Infernet client key over
//    "sub:<CoinPay sub>". Infernet computes the very same token for its own
//    ledgers (api/v1/network/leaderboard keys it with the caller's key), so a
//    person's c0mpute usage and Infernet spend meet under one token without a
//    raw id ever crossing the wire.
//  - A token is shown as a handle: "anon-" + 6 hex of a SECOND HMAC, keyed with
//    the leaderboard secret. Never a plain hash of an email, DID or sub (those
//    can be brute-forced from a list); without the secret a handle cannot be
//    tied back to anything, and the same person keeps the same handle.
//  - A board with fewer than MIN_PARTICIPANTS people is withheld, and a row
//    whose label only a handful of people share (a model, a tool) is folded
//    into "other", so no row describes one account.
//  - Customer money is shown in buckets, never to the cent.
import { createHmac } from "node:crypto";

/** Fewer people than this on a board (or behind a model/tool row) and it is withheld. */
export const MIN_PARTICIPANTS = 3;

/** Rows shown per board. */
export const TOP_N = 10;

const hmac = (key: string, msg: string) => createHmac("sha256", key).update(msg).digest("hex");

/** The person token for a CoinPay sub, as Infernet computes it for us. */
export function personToken(clientKey: string, coinpaySub: string): string {
  return hmac(clientKey, `sub:${coinpaySub}`);
}

/**
 * The leaderboard secret: LEADERBOARD_SECRET when set, otherwise derived from
 * the Infernet client key (a server secret that never leaves the box), so the
 * page needs no new setting to be anonymous. Null when neither exists, and
 * then every pseudonymous board is withheld.
 */
export function leaderboardSecret(env: { LEADERBOARD_SECRET?: string; INFERNET_CLIENT_KEY?: string }): string | null {
  if (env.LEADERBOARD_SECRET && env.LEADERBOARD_SECRET.length >= 16) return env.LEADERBOARD_SECRET;
  if (env.INFERNET_CLIENT_KEY && env.INFERNET_CLIENT_KEY.length >= 16) return hmac(env.INFERNET_CLIENT_KEY, "c0mpute-leaderboard-v1");
  return null;
}

/** "anon-7f3a2c": stable for one token, unlinkable without the secret. */
export function handleFor(secret: string, token: string): string {
  return `anon-${hmac(secret, `handle:${token}`).slice(0, 6)}`;
}

/** Customer spend as a range, never an exact figure. */
export function spendBucket(usd: number): string {
  if (!(usd > 0)) return "$0";
  if (usd < 0.01) return "< $0.01";
  if (usd < 0.1) return "$0.01 to $0.10";
  if (usd < 1) return "$0.10 to $1";
  if (usd < 10) return "$1 to $10";
  if (usd < 100) return "$10 to $100";
  if (usd < 1000) return "$100 to $1k";
  return "$1k+";
}

/** Counts rounded so a single account's exact activity is not readable off the board. */
export function roundCount(n: number): number {
  if (n < 10) return n;
  if (n < 100) return Math.round(n / 5) * 5;
  if (n < 1000) return Math.round(n / 10) * 10;
  return Math.round(n / 100) * 100;
}

/** A model or tool id safe to print: what the code knows, else "other". */
export function cleanLabel(raw: unknown): string {
  return typeof raw === "string" && /^[A-Za-z0-9._:/@-]{1,80}$/.test(raw) ? raw : "other";
}

export interface LabelRow {
  label: string;
  calls: number;
  users: number;
  tokens?: number;
}

/**
 * Fold every row fewer than MIN_PARTICIPANTS people share into one "other"
 * row (its user count is a sum, so it may count one person twice: shown as
 * a floor, never as exact). Busiest first.
 */
export function foldSmallLabels(rows: LabelRow[], min = MIN_PARTICIPANTS): LabelRow[] {
  const kept: LabelRow[] = [];
  const other: LabelRow = { label: "other", calls: 0, users: 0, tokens: 0 };
  for (const r of rows) {
    if (r.users >= min && r.label !== "other") kept.push(r);
    else {
      other.calls += r.calls;
      other.users += r.users;
      other.tokens = (other.tokens ?? 0) + (r.tokens ?? 0);
    }
  }
  kept.sort((a, b) => b.calls - a.calls || a.label.localeCompare(b.label));
  if (other.calls > 0) kept.push(other);
  return kept;
}

/** Fields that must never appear in anything the leaderboard publishes. */
export const FORBIDDEN_KEYS = [
  "email", "did", "coinpay_sub", "sub", "user_id", "wallet", "address", "payout_address", "ip",
  "api_key", "key", "token", "prompt", "messages", "content", "peer_id", "owner", "who", "provider",
] as const;

/** Patterns that must never appear in the published JSON (tests and the live check grep for them). */
export const PII_PATTERNS: RegExp[] = [
  /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/, // email
  /did:[a-z0-9]+:/i, // DID
  /\b0x[a-fA-F0-9]{40}\b/, // EVM address
  /\bbc1[02-9ac-hj-np-z]{25,62}\b/, // bitcoin bech32 address
  /\b[13](?=[1-9A-HJ-NP-Za-km-z]*[A-Z])[1-9A-HJ-NP-Za-km-z]{25,34}\b/, // bitcoin legacy address
  /\b(?=[1-9A-HJ-NP-Za-km-z]*[A-Z])(?=[1-9A-HJ-NP-Za-km-z]*[a-z])[1-9A-HJ-NP-Za-km-z]{32,64}\b/, // solana address / peer id (mixed-case base58)
  /\bc0k_[A-Za-z0-9_-]+/, // c0mpute API key
  /\bifc_[A-Za-z0-9]+/, // Infernet client key
  /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/i, // account uuid
  /\b\d{1,3}(\.\d{1,3}){3}\b/, // IPv4
];

/** Every forbidden key or PII pattern found in a value, for tests and a last check before publishing. */
export function piiFindings(value: unknown): string[] {
  const found: string[] = [];
  const walk = (v: unknown, path: string) => {
    if (Array.isArray(v)) v.forEach((x, i) => walk(x, `${path}[${i}]`));
    else if (v && typeof v === "object") {
      for (const [k, x] of Object.entries(v)) {
        if ((FORBIDDEN_KEYS as readonly string[]).includes(k)) found.push(`key ${path}.${k}`);
        walk(x, `${path}.${k}`);
      }
    } else if (typeof v === "string") {
      for (const re of PII_PATTERNS) if (re.test(v)) found.push(`value ${path} ~ ${re.source}`);
    }
  };
  walk(value, "$");
  return found;
}

/**
 * A copy of `value` with every string that looks like PII replaced by
 * "[hidden]". The builder never puts such a string there; this is the last
 * line of defence against a source sending one (a nickname that is a wallet
 * address), and it hides that one value instead of taking the page down.
 */
export function scrubPii<T>(value: T): { value: T; scrubbed: number } {
  let scrubbed = 0;
  const walk = (v: unknown): unknown => {
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x)]));
    if (typeof v === "string" && PII_PATTERNS.some((re) => re.test(v))) {
      scrubbed++;
      return "[hidden]";
    }
    return v;
  };
  return { value: walk(value) as T, scrubbed };
}
