// Who Infernet should meter or bill for a call made on a person's behalf.
//
// A person signed in with CoinPay is named by their CoinPay `sub`, which
// Infernet maps to their account and compute balance. A passkey/password-only
// account has no CoinPay identity, and until 2026-11-22 we sent nothing for
// them: Infernet then metered them by address, which is OUR server, so every
// such person shared one ten-a-day free allowance and new signups hit
// "free daily limit reached" almost at once.
//
// Now they are named `c0mpute:<user id>` (the opaque `u_...` id, never an
// email), signed with C0MPUTE_INFERNET_SUBJECT_SECRET so Infernet can tell we
// minted it: HMAC-SHA256 over "<subject>|<ts>|<request id>", sent as
// x-infernet-subject-ts / x-infernet-request-id / x-infernet-subject-sig.
// Infernet accepts it only with a valid signature, on our trusted key, within
// five minutes, and caps the total per day.

import { createHmac, randomBytes } from "node:crypto";

export const SIGNED_SUBJECT_PREFIX = "c0mpute:";

/** The subject for this person: their CoinPay sub when linked, else `c0mpute:<id>`. */
export function billingSubject(user: { id?: string | null; coinpay_sub?: string | null } | null | undefined): string | null {
  if (!user) return null;
  if (user.coinpay_sub) return user.coinpay_sub;
  return user.id ? `${SIGNED_SUBJECT_PREFIX}${user.id}` : null;
}

export function subjectSignature(secret: string, subject: string, ts: string, requestId: string): string {
  return createHmac("sha256", secret).update(`${subject}|${ts}|${requestId}`).digest("hex");
}

/**
 * The headers that name `subject` to Infernet. A `c0mpute:` subject is signed
 * (and dropped when no secret is configured, since Infernet would ignore it
 * anyway); a CoinPay subject is signed too when a secret is set, which costs
 * nothing and is ignored by Infernet.
 */
export function billedSubjectHeaders(
  subject: string | null | undefined,
  secret: string | null | undefined,
  opts: { now?: number; requestId?: string } = {},
): Record<string, string> {
  if (!subject) return {};
  const minted = subject.startsWith(SIGNED_SUBJECT_PREFIX);
  if (!secret || secret.length < 32) return minted ? {} : { "x-infernet-billed-subject": subject };
  const ts = String(Math.floor((opts.now ?? Date.now()) / 1000));
  const requestId = opts.requestId ?? randomBytes(12).toString("base64url");
  return {
    "x-infernet-billed-subject": subject,
    "x-infernet-subject-ts": ts,
    "x-infernet-request-id": requestId,
    "x-infernet-subject-sig": subjectSignature(secret, subject, ts, requestId),
  };
}
