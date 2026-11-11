/**
 * Infernet answers `{ data: {...} }` (lib/reservations/http.js ok()). Callers
 * read fields like total_spendable_usd and admin at the top level, so return
 * the inner object; a body that is already flat passes through unchanged.
 */
export function unwrapData(body: unknown): Record<string, unknown> | null {
  if (!body || typeof body !== "object") return null;
  const b = body as Record<string, unknown>;
  const inner = b.data;
  return inner && typeof inner === "object" && !Array.isArray(inner) ? (inner as Record<string, unknown>) : b;
}
