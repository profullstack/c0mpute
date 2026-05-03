/**
 * Minimal CoinPayments wrapper. We only need two flows for v1:
 *   1. Create a top-up invoice (customer)
 *   2. Trigger a payout (provider)
 *
 * IPN webhook validation lives in routes/webhooks.ts so it can use the raw
 * request body for HMAC verification.
 *
 * Stub today — fills in once we have sandbox creds.
 */

export interface TopupInvoice {
  invoiceId: string;
  payAddress: string;
  amount: number;
  currency: string;
  expiresAt: number;
}

export async function createTopupInvoice(_amountUsd: number): Promise<TopupInvoice> {
  throw new Error("CoinPayments integration not implemented yet");
}

export async function createPayout(
  _providerId: string,
  _amountUsd: number,
  _stablecoin: string,
): Promise<{ txId: string }> {
  throw new Error("CoinPayments integration not implemented yet");
}

/**
 * Validate the HMAC on a CoinPayments IPN webhook. Returns true iff the
 * signature is present and matches.
 */
export async function verifyIpn(
  rawBody: string,
  hmacHeader: string | null,
): Promise<boolean> {
  const secret = process.env.COINPAYMENTS_IPN_SECRET;
  if (!secret || !hmacHeader) return false;
  const enc = new TextEncoder();
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-512" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(rawBody));
  const hex = Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return timingSafeEqual(hex, hmacHeader.toLowerCase());
}

function timingSafeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}
