"use client";

import { useCallback, useEffect, useState } from "react";
import { SignInOptions } from "@/components/sign-in-options";
import { passkeyRegister, passkeysSupported } from "@/lib/accounts/passkey-client";
import { AlertsCard } from "./alerts-card";

type Passkey = { id: string; name: string | null; synced: boolean; created_at: string; last_used_at: string | null };
type Key = { id: string; label: string; prefix: string; created_at: string; last_used_at: string | null };
type FleetNode = {
  node: string;
  label: string | null;
  online: boolean;
  verified: boolean;
  tags: string[];
  version: string | null;
  earned_usd: { h24: number; d30: number; all: number };
};
type Account = {
  id: string;
  name: string | null;
  coinpay: { connected: boolean; did: string | null };
  sign_in: { coinpay: boolean; passkeys: Passkey[]; password: { username: string | null; deprecated: true } | null };
  keys: Key[];
  nodes: { totals: { nodes: number; online: number; verified: number; earned_usd: { h24: number; d30: number; all: number } }; list: FleetNode[] } | null;
  payouts: { ready: boolean; to?: string; wallets?: { chain: string; address: string; label?: string }[]; manage?: string; reason?: string };
  balance: { spendable_usd: string; earned_spendable_usd: string; prepaid_available_usd: string } | null;
};
type Token = { id: string; name: string; prefix: string; tags: string[]; uses: number; max_uses: number | null; expires_at: string | null; revoked_at: string | null };

const card = "border border-[var(--color-rule)] bg-[var(--color-card)] rounded-lg p-4 sm:p-5 space-y-3";
const btn = "border border-[var(--color-accent)] px-3 py-1.5 accent hover:bg-[var(--color-accent-dim)] disabled:opacity-50";
const quiet = "border border-[var(--color-rule)] px-3 py-1.5 text-[var(--color-dim)] hover:text-[var(--color-fg)] disabled:opacity-50";
const usd = (n: number) => `$${n.toFixed(4)}`;
const day = (s: string | null) => (s ? s.slice(0, 10) : "never");

async function call(path: string, method: string, body?: unknown) {
  const r = await fetch(path, { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j?.error?.message || `HTTP ${r.status}`);
  return j;
}

export function AccountApp({ connected }: { secure: boolean; connected: string | null }) {
  const [acct, setAcct] = useState<Account | null | undefined>(undefined);
  const [tokens, setTokens] = useState<Token[]>([]);
  const [msg, setMsg] = useState<string | null>(connected === "coinpay" ? "CoinPay connected." : null);
  const [err, setErr] = useState<string | null>(null);
  const [newKey, setNewKey] = useState<string | null>(null);
  const [newToken, setNewToken] = useState<{ token: string; install: string } | null>(null);

  const load = useCallback(async () => {
    const r = await fetch("/api/v1/account");
    if (r.status === 401) return setAcct(null);
    if (!r.ok) return setErr("Could not load your account. Try again in a moment.");
    setAcct(await r.json());
    const t = await fetch("/api/v1/fleet/tokens");
    if (t.ok) setTokens((await t.json()).tokens ?? []);
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const act = async (fn: () => Promise<unknown>, ok?: string) => {
    setErr(null);
    setMsg(null);
    try {
      await fn();
      if (ok) setMsg(ok);
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  if (acct === undefined) return <p className="text-[var(--color-dim)] text-sm">loading…</p>;
  if (acct === null)
    return (
      <div className="space-y-4 text-sm">
        <p>Sign in to see your account.</p>
        <SignInOptions next="/account" />
      </div>
    );

  const strong = acct.sign_in.coinpay || acct.sign_in.passkeys.length > 0;

  return (
    <div className="space-y-6 text-sm leading-6">
      {msg && <p className="border border-[var(--color-accent-dim)] p-3 accent">{msg}</p>}
      {err && (
        <p role="alert" className="border border-[var(--color-warn)] p-3 text-[var(--color-warn)]">
          {err}
        </p>
      )}
      {!strong && (
        <section role="note" className="border border-[var(--color-warn)] p-4 space-y-2">
          <p className="text-[var(--color-warn)] font-bold">Secure your account</p>
          <p>
            You signed in with a password, which is deprecated and will be removed. Add a passkey or connect CoinPay now so you keep a way in.
          </p>
          <div className="flex flex-wrap gap-3">
            <button className={btn} disabled={!passkeysSupported()} onClick={() => act(() => passkeyRegister(), "Passkey added.")}>
              add a passkey
            </button>
            <a className={btn} href="/api/auth/coinpay/login?link=1&next=/account">
              connect CoinPay
            </a>
          </div>
        </section>
      )}

      {/* identity */}
      <section className={card}>
        <h2 className="font-bold">identity</h2>
        <div className="grid sm:grid-cols-[10rem_1fr] gap-x-4 gap-y-1">
          <span className="text-[var(--color-dim)]">name</span>
          <span>
            {acct.name ?? <i className="text-[var(--color-dim)]">none</i>}{" "}
            <button
              className="underline text-[var(--color-dim)] ml-2"
              onClick={() => {
                const n = prompt("Display name", acct.name ?? "");
                if (n !== null) act(() => call("/api/v1/account", "PATCH", { name: n }));
              }}
            >
              edit
            </button>
          </span>
          <span className="text-[var(--color-dim)]">account id</span>
          <code className="break-all">{acct.id}</code>
          <span className="text-[var(--color-dim)]">CoinPay</span>
          {acct.coinpay.connected ? (
            <span className="break-all">
              connected · DID <code>{acct.coinpay.did}</code>
            </span>
          ) : (
            <span>
              not connected{" "}
              <a className="underline accent ml-2" href="/api/auth/coinpay/login?link=1&next=/account">
                Connect CoinPay
              </a>
            </span>
          )}
        </div>
      </section>

      {/* sign-in methods */}
      <section className={card}>
        <h2 className="font-bold">how you sign in</h2>
        <ul className="space-y-2">
          <li className="flex flex-wrap items-center justify-between gap-2">
            <span>CoinPay {acct.sign_in.coinpay ? <span className="accent">· on</span> : <span className="text-[var(--color-dim)]">· off</span>}</span>
            {acct.sign_in.coinpay ? (
              <button className={quiet} onClick={() => confirm("Disconnect CoinPay? Payouts stop until you connect it again.") && act(() => call("/api/v1/account?method=coinpay", "DELETE"), "CoinPay disconnected.")}>
                disconnect
              </button>
            ) : (
              <a className={btn} href="/api/auth/coinpay/login?link=1&next=/account">
                connect
              </a>
            )}
          </li>
          {acct.sign_in.passkeys.map((p) => (
            <li key={p.id} className="flex flex-wrap items-center justify-between gap-2">
              <span>
                passkey · {p.name ?? "passkey"} <span className="text-[var(--color-dim)]">{p.synced ? "(synced)" : "(this device)"} · added {day(p.created_at)} · last used {day(p.last_used_at)}</span>
              </span>
              <span className="flex gap-2">
                <button
                  className={quiet}
                  onClick={() => {
                    const n = prompt("Passkey name", p.name ?? "");
                    if (n) act(() => call("/api/v1/account/passkeys", "PATCH", { id: p.id, name: n }));
                  }}
                >
                  rename
                </button>
                <button className={quiet} onClick={() => confirm("Remove this passkey?") && act(() => call(`/api/v1/account/passkeys?id=${encodeURIComponent(p.id)}`, "DELETE"), "Passkey removed.")}>
                  remove
                </button>
              </span>
            </li>
          ))}
          <li>
            <button className={btn} disabled={!passkeysSupported()} onClick={() => act(() => passkeyRegister(), "Passkey added.")}>
              add a passkey
            </button>
          </li>
          <li className="pt-2 border-t border-[var(--color-line)] text-[var(--color-dim)]">
            password <span className="text-[var(--color-warn)]">(deprecated, will be removed)</span>:{" "}
            {acct.sign_in.password ? (
              <>
                username <code>{acct.sign_in.password.username}</code>{" "}
                <button className="underline ml-2" onClick={() => confirm("Remove your password?") && act(() => call("/api/v1/account?method=password", "DELETE"), "Password removed.")}>
                  remove
                </button>
              </>
            ) : (
              "none (and we recommend keeping it that way)"
            )}
          </li>
        </ul>
      </section>

      {/* payouts + balance */}
      <section className={card}>
        <h2 className="font-bold">earnings, payouts and balance</h2>
        {acct.payouts.ready ? (
          <p>
            Payouts go to <b>{acct.payouts.to}</b>. Manage your payout wallets at{" "}
            <a className="underline" href={acct.payouts.manage}>
              coinpayportal.com
            </a>
            .
            {acct.payouts.wallets && acct.payouts.wallets.length > 0 && (
              <span className="block mt-1 text-[var(--color-dim)]">
                CoinPay wallets:{" "}
                {acct.payouts.wallets.map((w) => (
                  <code key={w.address} className="mr-3 break-all">
                    {w.chain ? `${w.chain} ` : ""}
                    {w.address.length > 18 ? `${w.address.slice(0, 8)}…${w.address.slice(-6)}` : w.address}
                  </code>
                ))}
              </span>
            )}
          </p>
        ) : (
          <p className="border border-[var(--color-warn)] p-3">
            <b>Connect CoinPay to get paid.</b> Your nodes can earn and you can link them now, but earnings and payouts go to a CoinPay wallet.{" "}
            <a className="underline accent" href="/api/auth/coinpay/login?link=1&next=/account">
              Connect CoinPay
            </a>
          </p>
        )}
        {acct.nodes && (
          <p>
            Nodes earned (buyer-agreed, before the 5% fee): 24h <b>{usd(acct.nodes.totals.earned_usd.h24)}</b> · 30 days <b>{usd(acct.nodes.totals.earned_usd.d30)}</b> · all{" "}
            <b>{usd(acct.nodes.totals.earned_usd.all)}</b>. Details on <a href="/dashboard/nodes">your node dashboard</a>.
          </p>
        )}
        <p>
          Compute balance:{" "}
          {acct.balance ? (
            <>
              <b>${acct.balance.spendable_usd}</b> spendable (${acct.balance.prepaid_available_usd} topped up + ${acct.balance.earned_spendable_usd} from node earnings)
            </>
          ) : (
            <span className="text-[var(--color-dim)]">{acct.coinpay.connected ? "unavailable right now" : "needs CoinPay (chat runs on the free tier meanwhile)"}</span>
          )}
        </p>
      </section>

      {/* nodes */}
      <section className={card}>
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 className="font-bold">nodes</h2>
          <a href="/dashboard/nodes">manage your fleet →</a>
        </div>
        {acct.nodes && acct.nodes.list.length > 0 ? (
          <>
            <p className="text-[var(--color-dim)]">
              {acct.nodes.totals.nodes} nodes · {acct.nodes.totals.online} online · {acct.nodes.totals.verified} verified
            </p>
            <ul className="divide-y divide-[var(--color-line)]">
              {acct.nodes.list.slice(0, 12).map((n) => (
                <li key={n.node} className="py-1.5 flex flex-wrap gap-x-4 justify-between">
                  <span>
                    <span className={n.online ? "accent" : "text-[var(--color-dim)]"}>{n.online ? "●" : "○"}</span> <code>{n.node}</code> {n.label}
                    {n.tags.map((t) => (
                      <span key={t} className="ml-1 text-xs px-1.5 rounded bg-[var(--color-panel)]">
                        {t}
                      </span>
                    ))}
                  </span>
                  <span className="text-[var(--color-dim)]">
                    {n.verified ? "verified" : "unverified"} · {n.version ?? "?"} · 30d {usd(n.earned_usd.d30)}
                  </span>
                </li>
              ))}
            </ul>
          </>
        ) : (
          <p className="text-[var(--color-dim)]">No nodes linked yet.</p>
        )}
        <div className="space-y-1">
          <p className="font-bold">link a node</p>
          <pre className="text-xs overflow-x-auto bg-[var(--color-panel)] p-3 rounded">{`c0mpute login       # on the node: approve the code here
c0mpute node link   # the node signs a challenge with its identity key`}</pre>
        </div>
        <div className="space-y-2">
          <p className="font-bold">fleet enrollment tokens</p>
          <p className="text-[var(--color-dim)]">Link many boxes without a browser. A token can only link nodes to this account; it is shown once and stored hashed.</p>
          <form
            className="flex flex-wrap gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              const f = new FormData(e.currentTarget);
              act(async () => {
                const r = await call("/api/v1/fleet/tokens", "POST", { name: f.get("name"), expires: f.get("expires"), tags: String(f.get("tags") || "") || undefined });
                setNewToken({ token: r.token, install: r.install });
              });
            }}
          >
            <input name="name" placeholder="name (rack-a)" className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1" />
            <select name="expires" defaultValue="7d" className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1">
              <option value="1d">1 day</option>
              <option value="7d">7 days</option>
              <option value="30d">30 days</option>
              <option value="never">never</option>
            </select>
            <input name="tags" placeholder="tags (vast,home)" className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1" />
            <button className={btn}>create token</button>
          </form>
          {newToken && (
            <div className="border border-[var(--color-accent-dim)] p-3 space-y-1">
              <p>Copy it now; it is not shown again:</p>
              <code className="break-all block">{newToken.token}</code>
              <pre className="text-xs overflow-x-auto">{newToken.install}</pre>
            </div>
          )}
          {tokens.length > 0 && (
            <ul className="divide-y divide-[var(--color-line)]">
              {tokens.map((t) => (
                <li key={t.id} className="py-1.5 flex flex-wrap justify-between gap-2">
                  <span>
                    {t.name} <code className="text-[var(--color-dim)]">{t.prefix}…</code> {t.tags.join(",")}
                  </span>
                  <span className="text-[var(--color-dim)]">
                    used {t.uses}
                    {t.max_uses ? `/${t.max_uses}` : ""} · {t.revoked_at ? "revoked" : t.expires_at ? `expires ${day(t.expires_at)}` : "no expiry"}
                    {!t.revoked_at && (
                      <button className="underline ml-2" onClick={() => act(() => call(`/api/v1/fleet/tokens?id=${t.id}`, "DELETE"), "Token revoked.")}>
                        revoke
                      </button>
                    )}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>

      <AlertsCard />

      {/* keys */}
      <section className={card}>
        <h2 className="font-bold">API keys (CLI, API, MCP)</h2>
        <p className="text-[var(--color-dim)]">
          Use as <code>Authorization: Bearer c0k_…</code> on <code>/api/v1</code>, <code>/v1</code> (OpenAI-compatible) and <code>/api/mcp</code>. <code>c0mpute login</code> makes one for you.
        </p>
        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            const label = String(new FormData(e.currentTarget).get("label") || "api key");
            act(async () => setNewKey((await call("/api/v1/keys", "POST", { label })).key));
          }}
        >
          <input name="label" placeholder="label" className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1" />
          <button className={btn}>new key</button>
        </form>
        {newKey && (
          <p className="border border-[var(--color-accent-dim)] p-3">
            Copy it now; it is not shown again: <code className="break-all">{newKey}</code>
          </p>
        )}
        <ul className="divide-y divide-[var(--color-line)]">
          {acct.keys.map((k) => (
            <li key={k.id} className="py-1.5 flex flex-wrap justify-between gap-2">
              <span>
                {k.label} <code className="text-[var(--color-dim)]">{k.prefix}…</code>
              </span>
              <span className="text-[var(--color-dim)]">
                made {day(k.created_at)} · used {day(k.last_used_at)}
                <button className="underline ml-2" onClick={() => confirm(`Revoke ${k.label}?`) && act(() => call(`/api/v1/keys?id=${k.id}`, "DELETE"), "Key revoked.")}>
                  revoke
                </button>
              </span>
            </li>
          ))}
        </ul>
      </section>

      <button
        className={quiet}
        onClick={async () => {
          await fetch("/api/auth/logout", { method: "POST" });
          window.location.href = "/";
        }}
      >
        sign out
      </button>
    </div>
  );
}
