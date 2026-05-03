import { Hono } from "hono";

import { supabase } from "../lib/supabase.ts";

export const network = new Hono();

network.get("/health", async (c) => {
  const { count: peers } = await supabase()
    .from("providers")
    .select("*", { count: "exact", head: true })
    .eq("status", "online");
  return c.json({ peers_online: peers ?? 0, ok: true });
});

/**
 * Known-issues feed consumed by `quest doctor` to apply server-side
 * remediations without shipping a binary. Empty list = no current advisories.
 */
export const knownIssues = new Hono();

knownIssues.get("/", (c) => c.json({ items: [] }));
