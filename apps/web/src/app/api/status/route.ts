/**
 * GET /api/status — public network health JSON.
 *
 * Today: returns aggregate-only placeholder data while we stand up the
 * status aggregator service (DIP-0014). When deployed, the aggregator
 * will be a sibling Railway service running c0mpute in `verifier` mode
 * that subscribes to c0mpute/cap/v1 and c0mpute/jobs/* gossipsub
 * topics, aggregates, and exposes a JSON endpoint at the internal
 * Railway DNS name (status.railway.internal). This handler proxies to
 * it.
 *
 * Env var STATUS_AGGREGATOR_URL points at the aggregator. When unset
 * (today), we return a stub payload.
 *
 * Importantly: only AGGREGATE fields are exposed publicly. Individual
 * jobs, customer DIDs, and worker addresses do NOT appear here. See
 * DIP-0014 for the wire format and privacy model.
 */

import { NextResponse } from "next/server";

interface StatusPayload {
  ok: boolean;
  generated_at: string;
  network: {
    workers_online: number;
    workers_with_role: Record<string, number>;
    jobs_in_flight: number;
    jobs_completed_24h: number;
    avg_job_latency_seconds: number | null;
  };
  // No per-worker, per-job, per-customer fields here.
  source: "aggregator" | "stub";
}

const AGGREGATOR_URL = process.env.STATUS_AGGREGATOR_URL ?? null;

export async function GET() {
  if (AGGREGATOR_URL) {
    try {
      const r = await fetch(AGGREGATOR_URL, {
        // The aggregator is a sibling service on the same private
        // network — short timeout, no follow-redirects.
        signal: AbortSignal.timeout(2_000),
        cache: "no-store",
      });
      if (r.ok) {
        const data = (await r.json()) as Omit<StatusPayload, "source">;
        return NextResponse.json(
          { ...data, source: "aggregator" } satisfies StatusPayload,
          { headers: { "cache-control": "public, max-age=15" } },
        );
      }
    } catch {
      // fall through to stub
    }
  }

  const stub: StatusPayload = {
    ok: true,
    generated_at: new Date().toISOString(),
    network: {
      workers_online: 0,
      workers_with_role: {
        storage: 0,
        transcode: 0,
        gateway: 0,
        verifier: 0,
      },
      jobs_in_flight: 0,
      jobs_completed_24h: 0,
      avg_job_latency_seconds: null,
    },
    source: "stub",
  };
  return NextResponse.json(stub, {
    headers: { "cache-control": "public, max-age=30" },
  });
}
