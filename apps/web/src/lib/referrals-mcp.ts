// Node referral MCP tools (referral_link, referral_stats) for /api/mcp.
// No imports: the upstream passed in is already bound to the signed-in person
// (their CoinPay sub) through our trusted Infernet key; Infernet does every
// check. Commission = a percent of the house fee on paid jobs a referred node
// serves, never taken from the operator's earnings.

export type Upstream = (path: string, init?: RequestInit) => Promise<Response>;

export const REFERRAL_TOOLS = [
  {
    name: "referral_link",
    description:
      "Your node referral link (c0mpute.com/r/<code> and infernetprotocol.com/r/<code>) and the install commands that carry your code. Created once; your commission term is set then. Commission is a percent of the house fee on paid jobs your referred nodes serve, never taken from the operator's earnings.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "My referral link", readOnlyHint: false, destructiveHint: false, openWorldHint: false },
  },
  {
    name: "referral_stats",
    description: "Your node referrals: referred nodes, active nodes, commissions pending / confirmed / available / paid, payout address and recent commissions.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "My referral stats", readOnlyHint: true, openWorldHint: false },
  },
] as const;
export const REFERRAL_TOOL_NAMES = new Set<string>(REFERRAL_TOOLS.map((t) => t.name));

/** Run one referral MCP tool against the person-bound upstream. */
export async function referralTool(name: string, upstream: Upstream): Promise<{ text: string; isError?: boolean }> {
  const res =
    name === "referral_link"
      ? await upstream("/api/v1/referrals", { method: "POST", headers: { "content-type": "application/json" }, body: "{}", signal: AbortSignal.timeout(15_000) })
      : await upstream("/api/v1/referrals", { signal: AbortSignal.timeout(15_000) });
  const body = (await res.json().catch(() => ({}))) as { data?: Record<string, unknown>; error?: string };
  if (!res.ok) return { text: body.error || `referrals unavailable (HTTP ${res.status})`, isError: true };
  return { text: JSON.stringify(body.data ?? body, null, 2) };
}
