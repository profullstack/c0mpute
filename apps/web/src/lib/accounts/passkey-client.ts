"use client";
// Browser side of passkeys: ask the server for options, run the WebAuthn
// ceremony, send the answer back. Works in the installed PWA too (same origin).
import { browserSupportsWebAuthn, startAuthentication, startRegistration } from "@simplewebauthn/browser";

async function post(path: string, body: unknown) {
  const r = await fetch(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body ?? {}), credentials: "same-origin" });
  const j = (await r.json().catch(() => ({}))) as Record<string, unknown> & { error?: { message?: string } };
  if (!r.ok) throw new Error(j.error?.message || `HTTP ${r.status}`);
  return j;
}

export const passkeysSupported = () => typeof window !== "undefined" && browserSupportsWebAuthn();

const friendly = (e: unknown) => {
  const name = (e as { name?: string })?.name;
  if (name === "NotAllowedError") return new Error("The passkey prompt was closed or timed out.");
  if (name === "InvalidStateError") return new Error("This device already has a passkey for this account.");
  return e instanceof Error ? e : new Error(String(e));
};

/** Sign in with a passkey this device (or a synced keychain) holds. */
export async function passkeySignIn(): Promise<{ account: { id: string; name: string | null; coinpay: boolean } }> {
  const { options, ceremony } = (await post("/api/auth/passkey/login-options", {})) as { options: never; ceremony: string };
  let response;
  try {
    response = await startAuthentication({ optionsJSON: options });
  } catch (e) {
    throw friendly(e);
  }
  return (await post("/api/auth/passkey/login-verify", { ceremony, response })) as never;
}

/** Signed out: create an account with a passkey. Signed in: add a passkey to this account. */
export async function passkeyRegister(name?: string): Promise<{ created: boolean }> {
  const { options, ceremony } = (await post("/api/auth/passkey/register-options", { name })) as { options: never; ceremony: string };
  let response;
  try {
    response = await startRegistration({ optionsJSON: options });
  } catch (e) {
    throw friendly(e);
  }
  return (await post("/api/auth/passkey/register-verify", { ceremony, response, name })) as never;
}
