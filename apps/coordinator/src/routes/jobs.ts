import { Hono } from "hono";
import { z } from "zod";

import { supabase } from "../lib/supabase.ts";

export const jobs = new Hono();

/**
 * Worker claim. We use Postgres' RETURNING semantics under a single UPDATE
 * with a CTE-like select to keep the claim atomic — a worker can never claim
 * the same row twice.
 */
jobs.post("/claim", async (c) => {
  const providerId = c.req.header("x-quest-provider-id");
  if (!providerId) return c.json({ error: "missing provider" }, 401);

  // Supabase JS doesn't expose `for update skip locked` directly, so we go
  // through an RPC. The function is defined in 0001_init.sql.
  const { data, error } = await supabase().rpc("claim_next_job", {
    p_provider: providerId,
  });
  if (error) return c.json({ error: error.message }, 500);
  if (!data) return c.body(null, 204);
  return c.json(data);
});

const CompleteSchema = z.object({
  output_chunks: z.array(z.string()).min(1),
  output_bytes: z.number().int().nonnegative(),
  duration_seconds: z.number().nonnegative(),
  vmaf_self_score: z.number().nullable().optional(),
});

jobs.post("/:id/complete", async (c) => {
  const id = c.req.param("id");
  const body = await c.req.json().catch(() => null);
  const parsed = CompleteSchema.safeParse(body);
  if (!parsed.success) return c.json({ error: parsed.error.flatten() }, 400);

  const { error } = await supabase()
    .from("jobs")
    .update({
      status: "completed",
      result_hash: parsed.data.output_chunks[0],
      completed_at: new Date().toISOString(),
    })
    .eq("id", id);
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ ok: true });
});

jobs.post("/:id/fail", async (c) => {
  const id = c.req.param("id");
  const body = await c.req.json().catch(() => ({}));
  const error = String((body as { error?: string }).error ?? "unspecified");
  const { error: dbErr } = await supabase()
    .from("jobs")
    .update({ status: "failed", error })
    .eq("id", id);
  if (dbErr) return c.json({ error: dbErr.message }, 500);
  return c.json({ ok: true });
});
