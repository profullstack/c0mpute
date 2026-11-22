import { safeNext } from "@/lib/accounts/util";
import { PasswordForm } from "./password-form";

export const metadata = {
  title: "password sign-in (deprecated) — c0mpute",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default async function PasswordPage({ searchParams }: { searchParams: Promise<{ next?: string }> }) {
  const { next } = await searchParams;
  return (
    <div className="max-w-md mx-auto px-4 sm:px-6 py-16 space-y-6 text-sm leading-7">
      <h1 className="text-2xl font-bold accent">password sign-in</h1>
      <p role="note" className="border border-[var(--color-warn)] text-[var(--color-warn)] p-3">
        <b>Deprecated.</b> Passwords will be removed from c0mpute. Use them only on a device that cannot hold a passkey or open CoinPay, and
        add a passkey or connect CoinPay once you are in. <a href={`/signin?next=${encodeURIComponent(safeNext(next))}`}>Back to CoinPay and passkeys</a>
      </p>
      <PasswordForm next={safeNext(next)} />
    </div>
  );
}
