import { aggregatorUrls } from "@/lib/aggregators";
import { fetchSignal, parseSignalQuery } from "@/lib/autoscale";
import { json } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/autoscale/signal?node=<peer id or board id>&models=a,b&window_s=1200
 *
 * What `c0mpute autoscale` decides on: whether the node is online and busy
 * (jobs in flight, its last accepted and completed job) and the network's
 * demand for its models (offers in the window and the last 2 minutes, offers
 * still waiting for a node, offers nobody served), merged from every status
 * aggregator. Public: the same facts /nodes and /status already show.
 */
export async function GET(req: Request) {
  const q = parseSignalQuery(new URL(req.url).searchParams);
  if (typeof q === "string") return json({ error: "bad_request", detail: q }, 400);
  const signal = await fetchSignal(q, aggregatorUrls());
  if (!signal) return json({ error: "no_signal", detail: "no status aggregator answered" }, 503);
  return json(signal);
}
