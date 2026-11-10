import { ATTESTATION, validAttestation } from "@/lib/chat/adult";
import { currentUser, json, sameOrigin, store, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string) => json({ error: { message, type: "c0mpute_error", code: status } }, status);

/** GET: whether adult (18+) mode is on for this account, and the attestation text. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ adult_verified_at: user.adult_verified_at ?? null, attestation: ATTESTATION });
}

/**
 * POST { over_18: true, confirm: true }: the 18+ attestation (a checkbox and a
 * confirm in the chat settings), stored with its time. Only from a browser
 * session on this site: an API key cannot attest for a person.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  if (user.token_kind !== "session") return err(403, "adult mode is turned on in the c0mpute.com chat settings, not with an API key");
  if (!validAttestation(await req.json().catch(() => null))) return err(400, "tick the 18+ box and confirm");
  const at = await store().setAdultVerified(user.id, true);
  return json({ adult_verified_at: at });
}

/** DELETE: turn adult mode off (turning it on again needs a new attestation). */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  await store().setAdultVerified(user.id, false);
  return json({ adult_verified_at: null });
}
