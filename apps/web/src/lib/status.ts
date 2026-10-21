import "server-only";
import { fetchFromAggregators } from "./aggregators";

export interface StatusPayload {
  ok: boolean;
  generated_at: string;
  network: {
    workers_online: number;
    workers_with_role: Record<string, number>;
    workers_with_tag: Record<string, number>;
    jobs_in_flight: number;
    jobs_completed_24h: number;
    avg_job_latency_seconds: number | null;
    workload_types: Record<
      string,
      {
        jobs_in_flight: number;
        jobs_completed_24h: number;
        avg_latency_seconds: number | null;
      }
    >;
    /** `c0mpute bench` scores across online workers. Absent from older aggregators. */
    bench?: {
      workers_benchmarked: number;
      total_score: number;
      median_score: number | null;
      max_score: number | null;
    };
  };
  source: "aggregator" | "stub";
}


/**
 * `revalidate` (seconds) is for statically regenerated pages such as the home
 * page: a `no-store` fetch there throws Next's dynamic-usage error, which the
 * catch below would swallow into the stub. Dynamic pages leave it unset.
 */
export async function getStatusPayload(
  opts: { revalidate?: number } = {},
): Promise<StatusPayload> {
  const data = await fetchFromAggregators<Omit<StatusPayload, "source">>("/", opts);
  if (data) return { ...data, source: "aggregator" };
  return buildStub();
}

export function buildStub(): StatusPayload {
  return {
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
      workers_with_tag: {
        "c0mpute:role:storage": 0,
        "c0mpute:role:transcode": 0,
        "c0mpute:role:gateway": 0,
        "c0mpute:role:verifier": 0,
        "c0mpute:gpu:nvidia": 0,
        "c0mpute:gpu:amd": 0,
        "c0mpute:gpu:apple": 0,
        "c0mpute:cpu": 0,
      },
      jobs_in_flight: 0,
      jobs_completed_24h: 0,
      avg_job_latency_seconds: null,
      workload_types: {
        "ffmpeg.transcode": {
          jobs_in_flight: 0,
          jobs_completed_24h: 0,
          avg_latency_seconds: null,
        },
        "infernet.inference": {
          jobs_in_flight: 0,
          jobs_completed_24h: 0,
          avg_latency_seconds: null,
        },
      },
    },
    source: "stub",
  };
}
