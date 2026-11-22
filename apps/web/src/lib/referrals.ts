// Node referrals on c0mpute.com: thin client of Infernet, where c0mpute jobs
// settle (provider_earnings on the `c0mpute:<peer>` provider) and where the
// commission ledger lives. Every call goes through our trusted Infernet key
// and names the signed-in person by their CoinPay sub; Infernet does every
// check (codes, the A/B term, self-referral, accrual, payouts).
//
// Commission = a percent of the house fee on each paid job a referred node
// serves (5% of the fee by default), never taken from the operator's earnings.
import "server-only";
import { infernet } from "./chat/server";
import type { Upstream } from "./referrals-mcp";

export { REFERRAL_TOOLS, REFERRAL_TOOL_NAMES, referralTool, type Upstream } from "./referrals-mcp";

/** The client's address as nginx forwards it, for Infernet's self-referral check (only a hash of its /24 is kept). */
export function forwardedIp(req: Request): string | null {
  const ip = req.headers.get("x-real-ip") || req.headers.get("x-forwarded-for")?.split(",").pop()?.trim() || "";
  return /^[0-9a-fA-F:.]{3,64}$/.test(ip) ? ip : null;
}

/** Call Infernet's referral API for one person. */
export function referralUpstream(subject: string | null, ip: string | null = null): Upstream {
  return (path, init = {}) =>
    infernet(
      path,
      { ...init, headers: { ...(init.headers as Record<string, string>), ...(ip ? { "x-infernet-client-ip": ip } : {}) } },
      { subject },
    );
}

/** Pass an Infernet response straight back (status and JSON body). */
export async function relay(res: Response): Promise<Response> {
  const body = await res.text();
  return new Response(body || "{}", {
    status: res.status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

export type ReferralTerms = {
  commission_pct_of_house_fee: number;
  house_fee_pct: number | null;
  effective_pct_of_node_revenue: number | null;
  term: { arm: string; label: string | null; months: number | null } | null;
  min_payout_usd: number;
  auto_approve_max_usd: number;
};

/** Public lookup for /r/<code>: does the code exist, plus the terms (never who owns it). */
export async function lookupReferralCode(code: string): Promise<{ valid: boolean; code: string | null; terms: ReferralTerms | null } | null> {
  if (!/^[A-Za-z0-9]{6,16}$/.test(code)) return { valid: false, code: null, terms: null };
  try {
    const res = await infernet(`/api/v1/referrals/code/${encodeURIComponent(code.toLowerCase())}`, { signal: AbortSignal.timeout(8_000), cache: "no-store" });
    const body = (await res.json().catch(() => ({}))) as { data?: { valid: boolean; code: string | null; terms: ReferralTerms } };
    return body.data ?? null;
  } catch {
    return null;
  }
}

export function installCommands(code: string) {
  return {
    c0mpute: `curl -fsSL https://c0mpute.com/install.sh | C0MPUTE_REF=${code} sh`,
    c0mpute_register: `c0mpute worker register --ref ${code}`,
    infernet: `curl -fsSL https://infernetprotocol.com/install.sh | INFERNET_REFERRAL=${code} sh`,
  };
}
