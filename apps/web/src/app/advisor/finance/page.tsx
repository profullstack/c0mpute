import type { Metadata } from "next";
import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { pageOg } from "@/lib/og";
import { installLine, sanitizeRef } from "@/lib/nodes";
import { advisorLink, DEFAULT_REF, referralLink } from "@/lib/advisor";
import { COMMANDS, commandUsage, FINANCE_DISCLAIMER } from "@/lib/chat/command-registry";

// Agentic Advisor: Finance — the finance pack of prompt commands
// (commands/*.md, pack: finance) on c0mpute chat, its CLI, API, MCP, the npm
// package and the Claude Code plugin. ?ref=<handle> carries a referral handle
// from the person who shared the link (the existing /r/<code> program).

const TITLE = "Agentic Advisor: Finance";
const DESC =
  "Cash flow, runway, P&L, spend and forecasts from your own Mercury, CoinPay or QuickBooks data (or a CSV), answered by open models on the c0mpute network. Slash commands in c0mpute chat, the CLI, the API and MCP.";

export const metadata: Metadata = {
  title: `${TITLE} — c0mpute`,
  description: DESC,
  alternates: { canonical: "https://c0mpute.com/advisor/finance" },
  ...pageOg(TITLE, DESC, "/advisor/finance"),
};

type Props = { searchParams: Promise<{ ref?: string }> };

export default async function AdvisorFinancePage({ searchParams }: Props) {
  const ref = sanitizeRef((await searchParams).ref);
  const handle = ref ?? DEFAULT_REF;
  const pack = COMMANDS.filter((c) => c.pack === "finance");

  return (
    <div className="max-w-3xl mx-auto px-4 sm:px-6 py-16 space-y-10 text-sm leading-7">
      <header className="space-y-3">
        {ref ? <p className="comment">// shared by {ref}</p> : null}
        <h1 className="text-2xl font-bold accent">{TITLE}</h1>
        <p className="leading-relaxed">
          Ask where your money goes, how many months of runway you have, or what changed in last month&apos;s P&amp;L.
          The advisor reads your own connected accounts (Mercury, CoinPay, QuickBooks) or a CSV you paste, checks the
          numbers before it answers (row counts, gaps, totals that add up), and shows its arithmetic. It runs on
          open-weight models on the c0mpute peer-to-peer network.
        </p>
        <div className="flex flex-wrap gap-3 pt-2">
          <Link href="/chat" className="border border-[var(--color-accent)] px-4 py-2 accent no-underline hover:bg-[var(--color-accent-dim)]">
            open c0mpute chat and type /
          </Link>
          <a href="#install" className="border border-[var(--color-rule)] px-4 py-2 no-underline">
            CLI, API, MCP, Claude Code
          </a>
        </div>
      </header>

      <section className="space-y-3">
        <h2 className="font-semibold">[ commands ]</h2>
        <div className="overflow-x-auto">
          <table className="border-collapse w-full">
            <tbody>
              {pack.map((c) => (
                <tr key={c.name} className="border-b border-[var(--color-rule)] align-top">
                  <td className="py-2 pr-4 whitespace-nowrap accent">
                    <code>{commandUsage(c)}</code>
                  </td>
                  <td className="py-2">{c.description}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="text-[var(--color-dim)]">
          Example: <code>/runway</code>, <code>/spend last quarter</code>, or <code>/analyze which month had the most
          signups?</code> followed by a pasted CSV. <code>/board &lt;decision&gt;</code> hands your figures to the
          five-advisor <a href="https://board.saas.pit.world">Board as a Service</a> when that plugin is connected.
        </p>
      </section>

      <section className="space-y-3">
        <h2 className="font-semibold">[ your data ]</h2>
        <ul className="list-disc pl-5 space-y-1">
          <li>
            Mercury and CoinPay are chat plugins on by default; you connect each with your own sign-in (OAuth), from the
            plugins panel in <Link href="/chat">chat</Link>. Add QuickBooks or any SQL server as a plugin with{" "}
            <code>/plugin add &lt;url&gt;</code>. No account? Paste a CSV.
          </li>
          <li>Plugin tokens are stored sealed (AES-256-GCM) on your c0mpute account. Bank data stays in your bank; the advisor reads it through the plugin when you ask.</li>
          <li>Finance plugins are read-only by default: any tool that could move money is held for your explicit approval.</li>
          <li>
            To answer, the figures a tool returns go into the prompt for the model on a c0mpute node. Prompts are not
            stored by the public usage counters (they record the surface and model name only). For figures you would
            rather not send to a peer node, paste a summary, or use the API&apos;s house lane (
            <code>x-c0mpute-lane: house</code>): a Profullstack-operated node, nothing stored, paid only.
          </li>
        </ul>
      </section>

      <section id="install" className="space-y-3">
        <h2 className="font-semibold">[ everywhere you work ]</h2>
        <p className="comment">// web: type / in the chat box for the list</p>
        <p className="comment">// CLI</p>
        <CodeBlock>{`$ c0mpute chat login\n$ c0mpute chat "/runway"\n$ npx @profullstack/agentic-advisor-finance "/cashflow last 6 months"`}</CodeBlock>
        <p className="comment">// OpenAI-compatible API: the server expands the command</p>
        <CodeBlock>{`$ curl https://c0mpute.com/v1/chat/completions \\\n    -H "authorization: Bearer $C0MPUTE_API_KEY" -H "content-type: application/json" \\\n    -d '{"model":"auto","messages":[{"role":"user","content":"/analyze total signups?\\nmonth,signups\\nJan,120\\nFeb,135"}]}'`}</CodeBlock>
        <p className="comment">// MCP: the chat tool accepts the commands; list them at /api/v1/chat/commands</p>
        <CodeBlock>{`$ claude mcp add --transport http c0mpute https://c0mpute.com/api/mcp\n$ claude plugin marketplace add profullstack/claude-plugins\n$ claude plugin install agentic-advisor-finance@profullstack`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <h2 className="font-semibold">[ earn by sharing ]</h2>
        <p>
          Every link to the advisor carries a referral handle. Shared answers end with &quot;Analyzed on c0mpute&quot; and
          the link. With a referral code from <Link href="/dashboard/refer">dashboard/refer</Link>, you earn 50% of our
          5% fee on what the people you refer spend (2.5% of their spend), and 5% of our fee on paid jobs served by
          nodes you refer. It comes out of our cut: the people you refer pay the normal price and operators keep their
          share. A person counts as referred when they create their account from your link. This is a share of fees
          actually collected, not a promise of any income.
        </p>
        <CodeBlock>{advisorLink(handle)}</CodeBlock>
        <p className="text-[var(--color-dim)]">
          Run a node from this link (same referral): <code>{installLine(handle)}</code> · <a href={referralLink(handle)}>{referralLink(handle)}</a>
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)] border-t border-[var(--color-rule)] pt-4">
        {FINANCE_DISCLAIMER} Answers are written by AI models on peer nodes and can be wrong: check the arithmetic they
        show before you act. /analyze is adapted from Anthropic&apos;s knowledge-work-plugins (Apache-2.0).
      </p>
    </div>
  );
}
