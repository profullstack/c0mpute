// Shared bits of the /api/v1/storage routes.
import { STORAGE_CORS, STORAGE_SCOPE, StorageError } from "./storage";
import { currentUser, json, rateLimit } from "./server";

export { STORAGE_CORS };

export function storageJson(body: unknown, status = 200) {
  return json(body, status, STORAGE_CORS);
}

export function storageErr(e: unknown) {
  if (e instanceof StorageError) return storageJson({ error: { code: e.code, message: e.message } }, e.status);
  console.error("storage api", e);
  return storageJson({ error: { code: "server_error", message: "storage is unavailable right now" } }, 503);
}

export function storageUnauthorized() {
  return json(
    {
      error: {
        code: "unauthorized",
        message: `Send Authorization: Bearer <token>: a c0mpute.com OAuth token with the ${STORAGE_SCOPE} scope, or an API key (c0mpute chat login).`,
      },
    },
    401,
    { ...STORAGE_CORS, "www-authenticate": `Bearer realm="c0mpute", scope="${STORAGE_SCOPE}"`, "access-control-expose-headers": "www-authenticate" },
  );
}

/** A session, a full API key, or an OAuth token carrying storage:write. */
export async function storageUser(req: Request) {
  return currentUser(req, { scope: STORAGE_SCOPE });
}

/** Per-account request budget for the app-facing routes (separate from chat's). */
export function storageRate(userId: string) {
  return rateLimit(`storage:${userId}`, 600);
}

export function preflight() {
  return new Response(null, { status: 204, headers: { ...STORAGE_CORS, "access-control-max-age": "600" } });
}

/** The app's name, from the OAuth key's label ("PairUX (OAuth)" -> "PairUX"). */
export function appName(user: { token_kind: string; token_label?: string | null }): string | null {
  if (user.token_kind === "oauth") return (user.token_label ?? "").replace(/\s*\(OAuth\)\s*$/, "") || "OAuth app";
  if (user.token_kind === "key") return user.token_label ? `API key: ${user.token_label}` : "API key";
  return "c0mpute.com";
}
