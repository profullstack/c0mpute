// Server half of the /status earnings: reads the c0mpute job ledger and
// Infernet's public network earnings. Pure math is in ./network.
import "server-only";
import { infernet } from "../chat/server";
import { earningsStore } from "./recorder";
import { WINDOWS, parseInfernetEarnings, summariseNodeTotals, type C0mputeEarnings, type NetworkEarnings, type WindowKey } from "./network";

let cache: { at: number; value: NetworkEarnings } | null = null;

/** Both ledgers, cached 60 s. Either side is null when it cannot be read; the page still renders. */
export async function getNetworkEarnings(now = Date.now()): Promise<NetworkEarnings> {
  if (cache && now - cache.at < 60_000) return cache.value;
  const store = earningsStore();
  const keys = ["all", ...Object.keys(WINDOWS)] as WindowKey[];

  const c0mputeP = (async () => {
    const out = {} as Record<WindowKey, C0mputeEarnings>;
    for (const k of keys) {
      const since = k === "all" ? 0 : now - WINDOWS[k];
      out[k] = summariseNodeTotals(await store.nodeTotals(since));
    }
    return out;
  })().catch(() => null);

  const infernetP = (async () => {
    const r = await infernet("/api/v1/network/earnings", { signal: AbortSignal.timeout(8_000), cache: "no-store" });
    if (!r.ok) return null;
    return parseInfernetEarnings(await r.json());
  })().catch(() => null);

  const [c0mpute, inf, historyFrom] = await Promise.all([c0mputeP, infernetP, store.firstJobAt().catch(() => null)]);
  const value: NetworkEarnings = {
    generated_at: new Date(now).toISOString(),
    history_from: historyFrom,
    c0mpute,
    infernet: inf,
  };
  cache = { at: now, value };
  return value;
}
