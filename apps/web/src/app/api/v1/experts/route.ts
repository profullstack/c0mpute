import { experts, publicManifest } from "@/lib/experts/catalog";
import { expertPriceUsd, expertsConfig } from "@/lib/experts/config";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/experts: the Experts catalog (public, CORS *). Each entry is the
 * full signed manifest (format c0mpute.manifest/v1, docs/experts.md) plus the
 * store's price and the chat model id to use (`expert:<slug>`).
 * Asking needs an account: POST /v1/chat/completions with model "expert:<slug>",
 * or POST /api/v1/experts/<slug>/ask.
 */
export function GET() {
  const cfg = expertsConfig();
  const data = experts().map((m) => publicManifest(m, expertPriceUsd(m.pricing.monthly_usd, cfg)));
  return new Response(
    JSON.stringify({
      object: "list",
      count: data.length,
      pricing: { trial_days: cfg.trialDays, trial: "one free trial per CoinPay account, covering every expert", all_experts_usd: cfg.bundlePriceUsd, period_days: cfg.periodDays, currency: "USD", pay_with: "CoinPay" },
      data,
    }),
    { headers: { "content-type": "application/json", "cache-control": "public, max-age=300", "access-control-allow-origin": "*" } },
  );
}
