import { Hono } from "hono";
import { z } from "zod";

import { supabase } from "../lib/supabase.ts";

export const providers = new Hono();

const RegisterSchema = z.object({
  peer_id: z.string().min(8),
  hardware: z.record(z.string(), z.unknown()),
  capabilities: z.record(z.string(), z.unknown()),
});

providers.post("/register", async (c) => {
  const body = await c.req.json().catch(() => null);
  const parsed = RegisterSchema.safeParse(body);
  if (!parsed.success) return c.json({ error: parsed.error.flatten() }, 400);

  const ownerId = c.req.header("x-quest-user-id");
  if (!ownerId) return c.json({ error: "missing user" }, 401);

  const { data, error } = await supabase()
    .from("providers")
    .upsert(
      {
        owner_id: ownerId,
        peer_id: parsed.data.peer_id,
        hardware: parsed.data.hardware,
        capabilities: parsed.data.capabilities,
        status: "online",
        last_heartbeat: new Date().toISOString(),
      },
      { onConflict: "peer_id" },
    )
    .select("id")
    .single();
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ provider_id: data.id });
});

providers.post("/:id/heartbeat", async (c) => {
  const id = c.req.param("id");
  const body = await c.req.json().catch(() => ({}));
  const { error } = await supabase()
    .from("providers")
    .update({
      capabilities: (body as { capabilities?: unknown }).capabilities ?? null,
      status: "online",
      last_heartbeat: new Date().toISOString(),
    })
    .eq("id", id);
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ ok: true });
});
