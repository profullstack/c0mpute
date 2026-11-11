// Media jobs (images, music, albums, promo videos) started from the embedded
// chat. The chat keeps them client-side and polls /api/v1/<kind>/<id>; the
// app sees those polls on the chat's session (webRequest) and polls the same
// endpoints itself with the account key, so the Tasks tab shows them with
// progress even after the chat tab moved on.

import { readCreds } from "./account";

export type MediaKind = "images" | "music" | "album" | "promo";

export interface MediaJob {
  kind: MediaKind;
  id: string;
  at: number;
  status: "queued" | "running" | "done" | "failed" | "unknown";
  progress?: number;
  detail?: string;
  checked?: number;
}

/** A chat poll URL -> the job it is about. */
export function matchMediaPoll(url: string): { kind: MediaKind; id: string } | null {
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  const m = /^\/api\/v1\/(images|music|album|promo)\/([A-Za-z0-9_-]{6,80})\/?$/.exec(u.pathname);
  return m ? { kind: m[1] as MediaKind, id: m[2] } : null;
}

/** Status + progress out of a poll response (the shapes differ a little per kind). */
export function summarize(httpStatus: number, contentType: string, body: Record<string, unknown>): Pick<MediaJob, "status" | "progress" | "detail"> {
  // Finished images and songs come back as the file itself.
  if (httpStatus === 200 && !contentType.includes("json")) return { status: "done", progress: 1 };
  const err = body.error as { message?: string } | string | undefined;
  if (httpStatus >= 400 || body.status === "failed") {
    return { status: "failed", detail: typeof err === "string" ? err : (err?.message ?? `HTTP ${httpStatus}`) };
  }
  const raw = typeof body.status === "string" ? body.status : httpStatus === 200 ? "done" : "pending";
  const status: MediaJob["status"] = ["done", "completed", "succeeded"].includes(raw) ? "done" : body.stage === "queued" || raw === "queued" ? "queued" : "running";
  let progress: number | undefined;
  for (const k of ["progress", "percent", "pct"]) {
    const v = body[k];
    if (typeof v === "number" && Number.isFinite(v)) progress = v > 1 ? v / 100 : v;
  }
  // Promo: stages = { script: {state}, voice: {state}, ... }.
  const stages = body.stages;
  let stage: string | undefined;
  if (stages && typeof stages === "object") {
    const list = Object.entries(stages as Record<string, { state?: string; status?: string }>);
    if (list.length && progress === undefined) progress = list.filter(([, v]) => (v?.state ?? v?.status) === "done").length / list.length;
    stage = list.find(([, v]) => (v?.state ?? v?.status) === "running")?.[0];
  }
  if (typeof body.stage === "string") stage = body.stage;
  const position = typeof body.position === "number" ? ` (#${body.position} in line)` : "";
  return { status, progress: status === "done" ? 1 : progress, detail: stage ? stage + position : undefined };
}

export async function pollJob(job: MediaJob): Promise<MediaJob> {
  const creds = readCreds();
  if (!creds.key) return { ...job, status: "unknown", detail: "sign in to follow this job", checked: Date.now() };
  try {
    const res = await fetch(`${creds.url}/api/v1/${job.kind}/${job.id}`, {
      headers: { authorization: `Bearer ${creds.key}` },
      signal: AbortSignal.timeout(20_000),
    });
    const type = res.headers.get("content-type") ?? "";
    let body: Record<string, unknown> = {};
    if (type.includes("json")) body = ((await res.json().catch(() => ({}))) as Record<string, unknown>) ?? {};
    else await res.body?.cancel().catch(() => {});
    return { ...job, ...summarize(res.status, type, body), checked: Date.now() };
  } catch (e) {
    return { ...job, status: job.status === "done" ? "done" : "unknown", detail: e instanceof Error ? e.message : String(e), checked: Date.now() };
  }
}
