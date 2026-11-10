// Adult (18+) mode on c0mpute.com: who may use it, and the rules every
// generation request passes through first (lib/safety/content-policy.ts, the
// twin of the Rust gateway and worker guard, which run it again).
//
// - The child-safety guard is always on, adult mode or not.
// - Adult mode needs an 18+ attestation on the account (adult_verified_at),
//   a browser session to set it, and a paid compute balance: anonymous and
//   free-tier callers never get it.
// - Adult results never become og images, public galleries, shares or
//   /push sites, and the chat shows them blurred until clicked.

import { checkPolicy, recordRefusal, REFUSAL_MESSAGES, type PolicyRequest, type RefusalCode } from "@/lib/safety/content-policy";

export const ATTESTATION = "I am 18 or older, adult content is legal where I am, and I will not request content involving minors or real people.";

export type AdultCaller = { id: string; adult_verified_at?: string | null } | null;

export type Gate = { ok: true } | { ok: false; status: number; code: RefusalCode | "adult_needs_balance"; message: string };

const paid = (balance: Record<string, unknown> | null | undefined) => {
  const v = Number(balance?.total_spendable_usd ?? balance?.spendable_usd ?? 0);
  return Number.isFinite(v) && v > 0;
};

/**
 * Decide one generation request. `balance` is only read for adult requests.
 * Logs refusals by reason code with a hashed requester id, never the prompt.
 */
export function gateGeneration(caller: AdultCaller, req: PolicyRequest, balance?: Record<string, unknown> | null): Gate {
  const refuse = (code: RefusalCode, status: number): Gate => {
    recordRefusal(code, caller?.id ?? null);
    return { ok: false, status, code, message: REFUSAL_MESSAGES[code] };
  };
  if (req.adult) {
    if (!caller?.adult_verified_at) return refuse("adult_not_enabled", 403);
    if (!paid(balance)) {
      return { ok: false, status: 402, code: "adult_needs_balance", message: "Adult mode needs a paid compute balance (free-tier queries cannot use it)." };
    }
  }
  const code = checkPolicy(req);
  return code ? refuse(code, code === "adult_not_enabled" ? 403 : 422) : { ok: true };
}

/** The attestation body the UI posts: the checkbox and the confirm both true. */
export function validAttestation(body: unknown): boolean {
  const b = (body ?? {}) as Record<string, unknown>;
  return b.over_18 === true && b.confirm === true;
}
