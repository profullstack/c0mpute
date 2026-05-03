import { Hono } from "hono";
import { z } from "zod";

import { supabase } from "../lib/supabase.ts";

export const videos = new Hono();

const CreateVideoSchema = z.object({
  title: z.string().min(1).max(500),
  source_size_bytes: z.number().int().nonnegative(),
});

videos.post("/", async (c) => {
  const body = await c.req.json().catch(() => null);
  const parsed = CreateVideoSchema.safeParse(body);
  if (!parsed.success) {
    return c.json({ error: parsed.error.flatten() }, 400);
  }
  const ownerId = c.req.header("x-quest-user-id");
  if (!ownerId) return c.json({ error: "missing user" }, 401);

  const { data, error } = await supabase()
    .from("videos")
    .insert({
      owner_id: ownerId,
      title: parsed.data.title,
      source_size_bytes: parsed.data.source_size_bytes,
      status: "uploading",
    })
    .select("id")
    .single();
  if (error) return c.json({ error: error.message }, 500);

  // Real upload URL goes here once we wire S3/Supabase Storage. For now we
  // hand back a placeholder.
  return c.json({
    video_id: data.id,
    upload_url: `https://depin.quest/video/api/v1/videos/${data.id}/upload`,
  });
});

videos.post("/:id/finalize", async (c) => {
  const id = c.req.param("id");
  const { error } = await supabase()
    .from("videos")
    .update({ status: "queued" })
    .eq("id", id);
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ ok: true });
});

videos.get("/:id", async (c) => {
  const id = c.req.param("id");
  const { data, error } = await supabase()
    .from("videos")
    .select("*, renditions(*)")
    .eq("id", id)
    .single();
  if (error) return c.json({ error: error.message }, 404);
  return c.json(data);
});

videos.delete("/:id", async (c) => {
  const id = c.req.param("id");
  const { error } = await supabase()
    .from("videos")
    .update({ status: "deleted" })
    .eq("id", id);
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ ok: true });
});
