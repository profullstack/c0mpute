"use client";

import { useEffect, useState } from "react";
import { passkeyRegister, passkeySignIn, passkeysSupported } from "@/lib/accounts/passkey-client";

/**
 * The one sign-in block every page uses: CoinPay and passkeys first, the
 * deprecated password as a quiet secondary link. `next` is where to land.
 */
export function SignInOptions({ next = "/account", compact = false }: { next?: string; compact?: boolean }) {
  const [busy, setBusy] = useState<"" | "signin" | "create">("");
  const [err, setErr] = useState<string | null>(null);
  const [canPasskey, setCanPasskey] = useState(true);
  useEffect(() => setCanPasskey(passkeysSupported()), []);
  const go = () => {
    window.location.href = next;
  };

  return (
    <div className="space-y-3 text-sm">
      <div className="flex flex-wrap gap-3">
        <a
          href={`/api/auth/coinpay/login?next=${encodeURIComponent(next)}`}
          className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent hover:bg-[var(--color-accent-dim)]"
        >
          Sign in with CoinPay
        </a>
        <button
          type="button"
          disabled={!canPasskey || !!busy}
          onClick={async () => {
            setErr(null);
            setBusy("signin");
            try {
              await passkeySignIn();
              go();
            } catch (e) {
              setErr(e instanceof Error ? e.message : String(e));
            } finally {
              setBusy("");
            }
          }}
          className="border border-[var(--color-accent)] px-4 py-2 accent hover:bg-[var(--color-accent-dim)] disabled:opacity-50"
        >
          {busy === "signin" ? "waiting for passkey…" : "Sign in with a passkey"}
        </button>
      </div>
      {!compact && (
        <p className="text-[var(--color-dim)]">
          New here?{" "}
          <button
            type="button"
            disabled={!canPasskey || !!busy}
            className="underline accent disabled:opacity-50"
            onClick={async () => {
              setErr(null);
              setBusy("create");
              try {
                await passkeyRegister();
                go();
              } catch (e) {
                setErr(e instanceof Error ? e.message : String(e));
              } finally {
                setBusy("");
              }
            }}
          >
            {busy === "create" ? "creating…" : "create an account with a passkey"}
          </button>{" "}
          or sign in with CoinPay (a CoinPay DID is required; it becomes your identity on the network and is where payouts go).
        </p>
      )}
      {!canPasskey && <p className="text-[var(--color-dim)]">This browser cannot use passkeys; use CoinPay.</p>}
      {err && <p className="text-[var(--color-warn)]">{err}</p>}
      <p className="text-xs text-[var(--color-muted)]">
        <a href={`/signin/password?next=${encodeURIComponent(next)}`} className="!border-0 underline text-[var(--color-muted)]">
          Use a password (deprecated)
        </a>
      </p>
    </div>
  );
}
