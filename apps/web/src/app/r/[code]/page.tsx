import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { CodeBlock } from "@/components/CodeBlock";
import { installLine, sanitizeRef } from "@/lib/nodes";

type Props = { params: Promise<{ code: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const code = sanitizeRef((await params).code);
  return {
    title: code ? `${code} invited you to run a c0mpute node` : "c0mpute",
    description:
      "Run a c0mpute node with one command. The open compute network is aiming for 100,000 nodes so it can train and serve open-weight models in the open.",
    robots: { index: false, follow: true },
    alternates: { canonical: "https://c0mpute.com/" },
  };
}

export default async function ReferralPage({ params }: Props) {
  const code = sanitizeRef((await params).code);
  if (!code) notFound();

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8">
      <header className="space-y-3">
        <p className="comment">// invited by {code}</p>
        <h1 className="text-2xl font-bold accent">Run a c0mpute node</h1>
        <p className="leading-relaxed">
          c0mpute pools spare CPUs and GPUs into one open network. The goal is
          100,000 nodes, enough to train and serve open-weight models on
          hardware owned by the people running them.
        </p>
      </header>

      <section className="space-y-2">
        <p className="comment">// Linux, macOS, or Windows via WSL2. CPU or GPU.</p>
        <CodeBlock>{`$ ${installLine(code)}`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          The installer starts the worker for you. Your node then appears on{" "}
          <Link href="/nodes">the node board</Link> with <span className="accent">{code}</span>{" "}
          as the person who invited you. The handle is public and carries no
          payout.
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/">what c0mpute is</Link> · <Link href="/providers">what running a node involves</Link> ·{" "}
        <Link href="/nodes#refer">get your own link</Link>
      </p>
    </div>
  );
}
