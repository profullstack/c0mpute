// Spend referrals: remember which referral link a visitor arrived through.
//
// A referrer earns a share of the house fee on what the people they refer
// spend (Infernet's settings row `referrals`, 50% of the 5% fee by default).
// The person counts as referred when their account is first created, so the
// code has to survive from the landing page to the CoinPay sign-in: we keep
// the FIRST code seen (first touch) for 30 days in an httpOnly cookie, and the
// sign-in callback hands it to Infernet with the new account
// (`POST /api/v1/coinpay/link-trusted { ref }`). Infernet decides everything
// else: a real paid code, a new account that has never been charged, not the
// referrer themself, not a house account.
//
// The house handle `profullstack` and board handles (with - or _) are never
// stored: they carry no payout.

export const REF_COOKIE = "c0_ref";
export const REF_COOKIE_MAX_AGE = 30 * 24 * 60 * 60;
export const HOUSE_HANDLES = ["profullstack"] as const;

/** A paid referral code (Infernet's shape: 6-16 lowercase letters/digits), or null. */
export function paidRefCode(raw: string | null | undefined): string | null {
  if (typeof raw !== "string") return null;
  const code = raw.trim().toLowerCase();
  if (!/^[a-z0-9]{6,16}$/.test(code)) return null;
  if ((HOUSE_HANDLES as readonly string[]).includes(code)) return null;
  return code;
}

/** The code a request carries: `?ref=<code>` on any page, or the `/r/<code>` landing path. */
export function refFromUrl(url: URL): string | null {
  const q = paidRefCode(url.searchParams.get("ref"));
  if (q) return q;
  const m = /^\/r\/([^/]+)\/?$/.exec(url.pathname);
  return m ? paidRefCode(decodeURIComponent(m[1])) : null;
}

/** The stored first-touch code from a Cookie header, or null. */
export function refFromCookieHeader(header: string | null | undefined): string | null {
  if (!header) return null;
  for (const part of header.split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === REF_COOKIE) return paidRefCode(decodeURIComponent(v.join("=")));
  }
  return null;
}

/** The Set-Cookie options for a first touch (only set when no code is stored yet). */
export const REF_COOKIE_OPTIONS = {
  httpOnly: true,
  secure: true,
  sameSite: "lax" as const,
  path: "/",
  maxAge: REF_COOKIE_MAX_AGE,
};
