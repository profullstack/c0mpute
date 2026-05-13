import "server-only";

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
  };
  source: "aggregator" | "stub";
}

const AGGREGATOR_URL = process.env.STATUS_AGGREGATOR_URL ?? null;

export async function getStatusPayload(): Promise<StatusPayload> {
  if (AGGREGATOR_URL) {
    try {
      const r = await fetch(AGGREGATOR_URL, {
        signal: AbortSignal.timeout(2_000),
        cache: "no-store",
      });
      if (r.ok) {
        const data = (await r.json()) as Omit<StatusPayload, "source">;
        return { ...data, source: "aggregator" };
      }
    } catch {
      // fall through to stub
    }
  }

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
