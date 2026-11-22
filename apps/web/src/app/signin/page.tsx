import { SignInOptions } from "@/components/sign-in-options";
import { safeNext, SIGNIN_ERRORS } from "@/lib/accounts/util";

export const metadata = {
  title: "sign in — c0mpute",
  description: "Sign in to c0mpute with CoinPay or a passkey: one account for chat, your nodes, your fleet, the CLI, the API and MCP.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default async function SignInPage({ searchParams }: { searchParams: Promise<{ next?: string; error?: string }> }) {
  const { next: rawNext, error } = await searchParams;
  const next = safeNext(rawNext, "/account");
  const msg = error ? (SIGNIN_ERRORS[error] ?? `Sign-in failed (${error.slice(0, 40)}). Please try again.`) : null;
  return (
    <div className="max-w-xl mx-auto px-4 sm:px-6 py-16 space-y-8 text-sm leading-7">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">sign in</h1>
        <p className="comment">// one c0mpute account: chat, your nodes and fleet, earnings, the CLI, the API and MCP</p>
      </header>
      {msg && (
        <p role="alert" className="border border-[var(--color-warn)] text-[var(--color-warn)] p-3">
          {msg}
          {error === "no_did" && (
            <>
              {" "}
              <a href="https://coinpayportal.com" className="underline">
                Open CoinPay
              </a>
            </>
          )}
        </p>
      )}
      <SignInOptions next={next} />
      <section className="space-y-2 text-[var(--color-dim)]">
        <h2 className="text-[var(--color-fg)] font-bold">which should I use?</h2>
        <p>
          <b className="text-[var(--color-fg)]">CoinPay</b> if you run nodes: earnings and payouts go to your CoinPay wallet, and your CoinPay DID is
          your identity on the network. <b className="text-[var(--color-fg)]">A passkey</b> signs you in with your fingerprint, face or device PIN,
          nothing to remember. You can have both on one account: add either later on your account page.
        </p>
        <p>
          On a server with no browser? Run <code>c0mpute login</code>: it prints a code to approve here.
        </p>
      </section>
    </div>
  );
}
