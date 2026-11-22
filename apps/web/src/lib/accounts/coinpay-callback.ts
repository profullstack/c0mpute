// The CoinPay OAuth callback as a plain function, so state checks, PKCE
// plumbing, sub binding and the link-vs-sign-in decision are unit tested.

import { CoinpayTakenError, NoDidError, type Store, type User } from "../chat/store";
import { safeNext } from "./util";

export type SavedOAuth = { state?: string; verifier?: string; next?: string; link?: boolean };

export function readSaved(cookieHeader: string | null): SavedOAuth {
  const raw = (cookieHeader ?? "")
    .split(";")
    .map((s) => s.trim())
    .find((s) => s.startsWith("c0_oauth="));
  try {
    const v = JSON.parse(decodeURIComponent(raw?.slice("c0_oauth=".length) ?? ""));
    return v && typeof v === "object" ? (v as SavedOAuth) : {};
  } catch {
    return {};
  }
}

export type CallbackDeps = {
  users: Store;
  exchange: (code: string, verifier: string) => Promise<string>;
  userinfo: (accessToken: string) => Promise<{ sub: string; did?: string; name?: string; wallets?: unknown }>;
  /** Signed-in account in this browser, if any (only used when the flow was started with link=1). */
  current: User | null;
  afterSignIn?: (info: { sub: string; name?: string | null }) => Promise<unknown>;
};

/**
 * Where to send the browser next, and the account to start a session for
 * (null: no new session; the existing one stays when linking).
 */
export async function handleCallback(url: URL, saved: SavedOAuth, deps: CallbackDeps): Promise<{ redirect: string; user: User | null; linked?: boolean }> {
  const next = safeNext(saved.next, "/account");
  const fail = (code: string) => ({ redirect: `/signin?error=${encodeURIComponent(code)}&next=${encodeURIComponent(next)}`, user: null });
  const err = url.searchParams.get("error");
  if (err) return fail(/^[a-z_]{1,40}$/.test(err) ? err : "signin");
  const code = url.searchParams.get("code");
  // state must match the one this browser started with: no cookie, no match, no sign-in.
  if (!code || !saved.state || !saved.verifier || saved.state !== url.searchParams.get("state")) return fail("state");
  try {
    // The verifier is the one stored with this state; CoinPay checks it against the S256 challenge.
    const token = await deps.exchange(code, saved.verifier);
    const info = await deps.userinfo(token);
    if (saved.link) {
      if (!deps.current) return fail("session");
      const user = await deps.users.linkCoinpay(deps.current.id, info);
      await deps.users.setWallets(user.id, info.wallets).catch(() => {});
      await deps.afterSignIn?.(info);
      return { redirect: next.includes("?") ? `${next}&connected=coinpay` : `${next}?connected=coinpay`, user: null, linked: true };
    }
    // Sign-in binds by sub only (never by the unverified CoinPay email).
    const user = await deps.users.upsertCoinpayUser(info);
    await deps.users.setWallets(user.id, info.wallets).catch(() => {});
    await deps.afterSignIn?.(info);
    return { redirect: next, user };
  } catch (e) {
    if (e instanceof NoDidError) return fail("no_did");
    if (e instanceof CoinpayTakenError) return fail("coinpay_taken");
    console.error("coinpay callback failed", e);
    return fail("signin");
  }
}
