import { Hono } from "hono";

import { supabase } from "../lib/supabase.ts";

export const earnings = new Hono();

earnings.get("/:id/earnings", async (c) => {
  const id = c.req.param("id");
  const { data, error } = await supabase()
    .from("earnings")
    .select("*")
    .eq("provider_id", id)
    .order("created_at", { ascending: false })
    .limit(500);
  if (error) return c.json({ error: error.message }, 500);
  return c.json({ items: data });
});
