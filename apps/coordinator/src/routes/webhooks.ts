import { Hono } from "hono";

import { verifyIpn } from "../lib/coinpayments.ts";
import { supabase } from "../lib/supabase.ts";

export const webhooks = new Hono();

webhooks.post("/coinpayments", async (c) => {
  const raw = await c.req.text();
  const ok = await verifyIpn(raw, c.req.header("hmac") ?? null);
  if (!ok) return c.json({ error: "invalid signature" }, 401);

  // CoinPayments IPN payloads are application/x-www-form-urlencoded
  const params = new URLSearchParams(raw);
  const txn = params.get("txn_id");
  const status = Number(params.get("status") ?? "0");
  const amount = Number(params.get("amount1") ?? "0");
  const currency = params.get("currency1") ?? "USD";
  const customerId = params.get("custom"); // we set this when creating the invoice

  if (!txn || !customerId) return c.json({ error: "missing fields" }, 400);

  // status >= 100 means complete in CoinPayments' scheme
  if (status >= 100) {
    await supabase().from("billing").insert({
      customer_id: customerId,
      type: "topup",
      amount_usd: amount,
      reference: { coinpayments_txn: txn, currency },
      coinpayments_tx: txn,
    });
  }
  return c.json({ ok: true });
});
