import { Hono } from "hono";
import { z } from "zod";

import { supabase } from "../lib/supabase.ts";
import { createPayout, createTopupInvoice } from "../lib/coinpayments.ts";

export const billing = new Hono();

const TopupSchema = z.object({ amount_usd: z.number().positive() });

billing.post("/topup", async (c) => {
  const body = await c.req.json().catch(() => null);
  const parsed = TopupSchema.safeParse(body);
  if (!parsed.success) return c.json({ error: parsed.error.flatten() }, 400);

  try {
    const invoice = await createTopupInvoice(parsed.data.amount_usd);
    return c.json(invoice);
  } catch (err) {
    return c.json(
      { error: err instanceof Error ? err.message : String(err) },
      501,
    );
  }
});

const WithdrawSchema = z.object({
  amount_usd: z.number().positive(),
  stablecoin: z.string().min(2).max(10),
});

billing.post("/withdraw", async (c) => {
  const providerId = c.req.header("x-quest-provider-id");
  if (!providerId) return c.json({ error: "missing provider" }, 401);

  const body = await c.req.json().catch(() => null);
  const parsed = WithdrawSchema.safeParse(body);
  if (!parsed.success) return c.json({ error: parsed.error.flatten() }, 400);

  try {
    const tx = await createPayout(
      providerId,
      parsed.data.amount_usd,
      parsed.data.stablecoin,
    );
    await supabase().from("earnings").insert({
      provider_id: providerId,
      type: "withdraw",
      amount_usd: -parsed.data.amount_usd,
      amount_stable: -parsed.data.amount_usd,
      stablecoin: parsed.data.stablecoin,
    });
    return c.json(tx);
  } catch (err) {
    return c.json(
      { error: err instanceof Error ? err.message : String(err) },
      501,
    );
  }
});
