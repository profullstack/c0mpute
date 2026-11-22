// Server wiring for accounts: passkeys, passwords, node links, fleet. One
// identity across web, PWA, desktop, TUI, CLI, API and MCP: the same
// chat_users row and the same c0s_/c0k_ tokens everywhere.
import "server-only";
import * as webauthn from "@simplewebauthn/server";
import { cloudSql, currentUser, env, SESSION_COOKIE, siteUrl, store, bearer } from "../chat/server";
import type { User } from "../chat/store";
import { earningsStore } from "../earnings/server";
import { createFleetService, signingKey, type FleetService } from "./fleet";
import { fleetView } from "./fleet-view";
import { createLinkService, type Linker } from "./link";
import { createPasskeys, relyingParty, type Passkeys } from "./passkeys";
import { createAccountStore, ENROLL_PREFIX, type AccountStore } from "./store";

let _accounts: AccountStore | null = null;
export const accountStore = () => (_accounts ??= createAccountStore(cloudSql));

let _passkeys: Passkeys | null = null;
export const passkeys = () =>
  (_passkeys ??= createPasskeys({
    users: store(),
    accounts: accountStore(),
    lib: webauthn,
    // Extra origins (comma separated) for a staging host; production is the site itself.
    rp: relyingParty(siteUrl(), env("C0MPUTE_WEBAUTHN_ORIGINS").split(",").filter(Boolean)),
  }));

let _fleet: FleetService | null = null;
export const fleetService = () =>
  (_fleet ??= createFleetService({
    sql: cloudSql,
    accounts: accountStore(),
    site: siteUrl(),
    // An explicit key (PKCS#8 base64) wins; otherwise the one kept in the site database.
    key: env("C0MPUTE_FLEET_SIGNING_KEY") ? signingKey(env("C0MPUTE_FLEET_SIGNING_KEY")) : undefined,
  }));

export const linkService = () => createLinkService({ accounts: accountStore(), earnings: earningsStore(), site: siteUrl() });

/** The session cookie a sign-in sets (Secure, HttpOnly, SameSite=Lax, 30 days). */
export const sessionCookie = (token: string) => `${SESSION_COOKIE}=${token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=${30 * 86400}`;

/** Sign `user` in: a new browser session, returned as a Set-Cookie value. */
export async function signInCookie(user: User): Promise<string> {
  return sessionCookie(await store().createSession(user.id));
}

/**
 * Who may link a node: a signed-in account (API key or browser session), or a
 * fleet enrollment token (`Authorization: Bearer c0e_...`), which can do
 * nothing else.
 */
export async function currentLinker(req: Request): Promise<Linker | null> {
  const b = bearer(req);
  if (b?.startsWith(ENROLL_PREFIX)) {
    const t = await accountStore().enrollTokenOwner(b);
    return t ? { kind: "enroll", userId: t.user_id, tokenHash: t.hash, tokenName: t.name, tags: t.tags } : null;
  }
  const user = await currentUser(req);
  return user ? { kind: "user", userId: user.id, via: user.token_kind === "session" ? "web" : "key" } : null;
}

/** How the account can sign in, for the account page and the "keep a way in" checks. */
export async function signInMethods(user: User) {
  const [keys, hasPassword] = await Promise.all([accountStore().passkeys(user.id), store().hasPassword(user.id)]);
  return {
    coinpay: !!user.coinpay_sub,
    passkeys: keys.map((k) => ({ id: k.credential_id, name: k.name, synced: k.backed_up, created_at: k.created_at, last_used_at: k.last_used_at })),
    password: hasPassword ? { username: user.username ?? null, deprecated: true } : null,
  };
}

/** Removing one sign-in method must leave another. */
export async function keepsAWayIn(user: User, removing: { coinpay?: boolean; passkey?: string; password?: boolean }): Promise<boolean> {
  const m = await signInMethods(user);
  const coinpay = m.coinpay && !removing.coinpay;
  const keys = m.passkeys.filter((k) => k.id !== removing.passkey).length;
  const password = !!m.password && !removing.password;
  return coinpay || keys > 0 || password;
}

/** The signed-in operator's fleet: every node on the account, its status, earnings and queued commands. */
export async function fleetFor(userId: string) {
  const { nodes, totals } = await fleetView(userId, { accounts: accountStore(), fleet: fleetService(), earnings: earningsStore() });
  return { nodes, totals };
}
