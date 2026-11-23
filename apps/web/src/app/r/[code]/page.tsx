import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { CodeBlock } from "@/components/CodeBlock";
import { installLine, sanitizeRef } from "@/lib/nodes";
import { pageOg } from "@/lib/og";
import { lookupReferralCode } from "@/lib/referrals";
import { ReferralTerms } from "@/components/referral-terms";

type Props = { params: Promise<{ code: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const code = sanitizeRef((await params).code);
  const title = code ? `${code} invited you to c0mpute` : "c0mpute: the open compute network";
  const description =
    "Use open models on c0mpute, or run a node with one command. The open compute network is aiming for 100,000 nodes so it can train and serve open-weight models in the open.";
  return {
    title,
    description,
    robots: { index: false, follow: true },
    alternates: { canonical: "https://c0mpute.com/" },
    ...pageOg(title, description, `/r/${code ?? ""}`),
  };
}

export default async function ReferralPage({ params }: Props) {
  const code = sanitizeRef((await params).code);
  if (!code) notFound();
  // A paid node-referral code (c0mpute.com/dashboard/refer) earns its owner a
  // share of the house fee; any other handle is just a public "invited by".
  const paid = /^[a-z0-9]{6,16}$/.test(code) ? await lookupReferralCode(code) : null;
  const isPaid = Boolean(paid?.valid);

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8">
      <header className="space-y-3">
        <p className="comment">// invited by {code}</p>
        <h1 className="text-2xl font-bold accent">Use c0mpute, or run a node</h1>
        <p className="leading-relaxed">
          c0mpute pools spare CPUs and GPUs into one open network. The goal is
          100,000 nodes, enough to train and serve open-weight models on
          hardware owned by the people running them.
        </p>
      </header>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">use it</h2>
        <p className="leading-relaxed">
          Chat with open models, call them from the <Link href="/docs">API</Link>, make images and music. Sign in with CoinPay to
          get started.
        </p>
        <p>
          <a href={`/api/auth/coinpay/login?next=${encodeURIComponent("/chat")}`} className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent">
            sign in and open chat
          </a>
        </p>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          {isPaid
            ? "This is a referral link. If you create your account from it, the person who invited you gets a share of our house fee on what you spend. It comes out of our cut: you pay the normal price."
            : "The handle is public and carries no payout."}
        </p>
      </section>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">or run a node</h2>
        <p className="comment">// Linux, macOS, or Windows via WSL2. CPU or GPU.</p>
        <CodeBlock>{`$ ${installLine(code)}`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          The installer starts the worker for you. Your node then appears on{" "}
          <Link href="/nodes">the node board</Link> with <span className="accent">{code}</span>{" "}
          as the person who invited you.{" "}
          {isPaid
            ? "This is a referral code: the person who invited you gets a share of the house fee on the paid jobs your node serves. It comes out of the house's cut, so nothing changes about what you earn."
            : null}
        </p>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Already installed? <code>c0mpute worker register --ref {code}</code>
        </p>
      </section>

      {isPaid && paid?.terms ? <ReferralTerms terms={paid.terms} /> : null}

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/">what c0mpute is</Link> · <Link href="/providers">what running a node involves</Link> ·{" "}
        <Link href="/dashboard/refer">get your own referral link</Link>
      </p>
    </div>
  );
}
