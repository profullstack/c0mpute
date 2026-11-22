// Passkeys (WebAuthn) for c0mpute accounts, on @simplewebauthn/server (the
// house pattern): discoverable credentials, user verification preferred.
//
// Two ceremonies, each a pair of calls with a one-shot server challenge:
//   register: signed in -> add a passkey to this account;
//             signed out -> create a new account whose first way in is the passkey.
//   login:    no username; the browser offers the passkeys it holds for c0mpute.com.
//
// The WebAuthn verify functions are injected so tests can drive the flow
// without a real authenticator; production passes @simplewebauthn/server's.

import { randomBytes } from "node:crypto";
import type {
  generateAuthenticationOptions as GenAuth,
  generateRegistrationOptions as GenReg,
  verifyAuthenticationResponse as VerifyAuth,
  verifyRegistrationResponse as VerifyReg,
} from "@simplewebauthn/server";
import type { Store, User } from "../chat/store";
import type { AccountStore } from "./store";

export type WebAuthnLib = {
  generateRegistrationOptions: typeof GenReg;
  verifyRegistrationResponse: typeof VerifyReg;
  generateAuthenticationOptions: typeof GenAuth;
  verifyAuthenticationResponse: typeof VerifyAuth;
};

export type RelyingParty = { id: string; name: string; origins: string[] };

/** The relying party for a site URL: rpID is its host, the origin is the site itself. */
export function relyingParty(siteUrl: string, extraOrigins: string[] = []): RelyingParty {
  const u = new URL(siteUrl);
  return { id: u.hostname, name: "c0mpute", origins: [u.origin, ...extraOrigins] };
}

export class PasskeyError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

const b64url = (b: Uint8Array) => Buffer.from(b).toString("base64url");
const fromB64url = (s: string) => new Uint8Array(Buffer.from(s, "base64url"));

export function createPasskeys(deps: { users: Store; accounts: AccountStore; lib: WebAuthnLib; rp: RelyingParty }) {
  const { users, accounts, lib, rp } = deps;

  return {
    /** Options for navigator.credentials.create(), plus the ceremony id to send back. */
    async registerOptions(user: User | null, opts: { name?: string | null } = {}) {
      const existing = user ? await accounts.passkeys(user.id) : [];
      // The WebAuthn user handle: random, never the account id or a name, so a
      // credential reveals nothing about the account it belongs to.
      const userID = randomBytes(16);
      const label = (user?.name || opts.name || "c0mpute account").slice(0, 64);
      const options = await lib.generateRegistrationOptions({
        rpName: rp.name,
        rpID: rp.id,
        userName: label,
        userDisplayName: label,
        userID,
        attestationType: "none",
        excludeCredentials: existing.map((p) => ({ id: p.credential_id, transports: p.transports })),
        authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "preferred" },
      });
      const ceremony = await accounts.putChallenge("webauthn-register", options.challenge, { userId: user?.id ?? null });
      return { options, ceremony };
    },

    /**
     * Check the authenticator's answer and save the passkey. Signed in when the
     * ceremony started: the passkey joins that account (and only that one: the
     * account is read from the stored ceremony, not the request). Signed out:
     * a new account is made. Returns the account and whether it is new.
     */
    async registerVerify(ceremony: string, response: unknown, opts: { current: User | null; name?: string | null }) {
      const c = await accounts.takeChallenge("webauthn-register", ceremony);
      if (!c) throw new PasskeyError("this passkey prompt expired or was already used; try again");
      // A ceremony started signed in must finish as the same account.
      if (c.user_id && c.user_id !== opts.current?.id) throw new PasskeyError("signed-in account changed during the passkey prompt", 403);
      let v;
      try {
        v = await lib.verifyRegistrationResponse({
          response: response as never,
          expectedChallenge: c.challenge,
          expectedOrigin: rp.origins,
          expectedRPID: rp.id,
          requireUserVerification: false,
        });
      } catch (e) {
        throw new PasskeyError(`passkey not accepted: ${e instanceof Error ? e.message : "verification failed"}`);
      }
      if (!v.verified || !v.registrationInfo) throw new PasskeyError("passkey not accepted");
      const { credential, credentialDeviceType, credentialBackedUp } = v.registrationInfo;
      if (await accounts.passkey(credential.id)) throw new PasskeyError("this passkey is already registered", 409);
      let user: User | null = c.user_id ? await users.userById(c.user_id) : null;
      let created = false;
      if (!user) {
        user = await users.createLocalUser(opts.name?.trim() || null);
        created = true;
      }
      await accounts.addPasskey({
        credential_id: credential.id,
        user_id: user.id,
        public_key: b64url(credential.publicKey),
        counter: credential.counter,
        transports: (credential.transports as string[] | undefined) ?? [],
        device_type: credentialDeviceType,
        backed_up: credentialBackedUp,
        name: (opts.name?.trim() || (credentialBackedUp ? "synced passkey" : "device passkey")).slice(0, 80),
      });
      return { user, created };
    },

    /** Options for navigator.credentials.get(): no allowCredentials, so the browser offers discoverable passkeys. */
    async loginOptions() {
      const options = await lib.generateAuthenticationOptions({ rpID: rp.id, userVerification: "preferred", allowCredentials: [] });
      const ceremony = await accounts.putChallenge("webauthn-login", options.challenge);
      return { options, ceremony };
    },

    /** Check an assertion; the account is whoever owns the credential. */
    async loginVerify(ceremony: string, response: { id?: unknown } & Record<string, unknown>): Promise<User> {
      const c = await accounts.takeChallenge("webauthn-login", ceremony);
      if (!c) throw new PasskeyError("this passkey prompt expired or was already used; try again");
      const id = typeof response?.id === "string" ? response.id : "";
      const pk = id ? await accounts.passkey(id) : null;
      if (!pk) throw new PasskeyError("this passkey is not registered on c0mpute (it may have been removed)", 401);
      let v;
      try {
        v = await lib.verifyAuthenticationResponse({
          response: response as never,
          expectedChallenge: c.challenge,
          expectedOrigin: rp.origins,
          expectedRPID: rp.id,
          credential: { id: pk.credential_id, publicKey: fromB64url(pk.public_key), counter: pk.counter, transports: pk.transports as never },
          requireUserVerification: false,
        });
      } catch (e) {
        throw new PasskeyError(`passkey not accepted: ${e instanceof Error ? e.message : "verification failed"}`, 401);
      }
      if (!v.verified) throw new PasskeyError("passkey not accepted", 401);
      await accounts.usePasskey(pk.credential_id, v.authenticationInfo.newCounter);
      const user = await users.userById(pk.user_id);
      if (!user) throw new PasskeyError("the account for this passkey no longer exists", 401);
      await users.touchLogin(user.id);
      return user;
    },
  };
}

export type Passkeys = ReturnType<typeof createPasskeys>;
