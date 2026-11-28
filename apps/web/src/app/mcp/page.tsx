import type { Metadata } from "next";
import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { pageOg } from "@/lib/og";

// Landing page for chat plugins (bring your own MCP servers), the ad and short
// destination. A referral arrives as ?ref= or ?referral= (proxy.ts keeps it).

const title = "Bring your own MCP servers to open models";
const description =
  "Add any remote MCP server to c0mpute chat with an OAuth sign-in or an API key, and open models on a peer-to-peer GPU network call its tools. Web, CLI, API and MCP. Pay as you go with crypto or PayPal.";

export const metadata: Metadata = {
  title,
  description,
  alternates: { canonical: "https://c0mpute.com/mcp" },
  ...pageOg(title, description, "/mcp"),
};

const SIGN_IN = `/api/auth/coinpay/login?next=${encodeURIComponent("/chat")}`;

export default function McpPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-3">
        <p className="comment">// chat plugins</p>
        <h1 className="text-2xl font-bold accent">Your MCP servers, inside c0mpute chat</h1>
        <p className="leading-relaxed">
          Add any remote MCP server and the open models on the c0mpute network can use its tools: look things up,
          read your data, make designs, run scans. c0mpute runs the tool loop for you, so it works the same in the
          browser, the CLI, the desktop app, the API and over MCP.
        </p>
        <p>
          <a href={SIGN_IN} className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent">
            sign in with CoinPay and add one
          </a>
        </p>
      </header>

      <section className="space-y-3">
        <h2 className="text-lg font-bold accent">add a server three ways</h2>
        <p className="comment">// in the chat</p>
        <CodeBlock>{`/plugin add https://example.com/mcp
/plugin key example        # if it takes an API key (asked in a box, never saved in the chat)
/plugin rename example Example Docs
/plugin off example | /plugin rm example`}</CodeBlock>
        <p className="comment">// in a terminal</p>
        <CodeBlock>{`c0mpute chat plugins add https://example.com/mcp --key
c0mpute chat "make me an A4 poster for Friday's meetup"`}</CodeBlock>
        <p className="comment">// over the API</p>
        <CodeBlock>{`POST   /api/v1/chat/plugins          {"url": "https://example.com/mcp", "api_key": "…"}
PATCH  /api/v1/chat/plugins/example  {"title": "Example", "enabled": true}
DELETE /api/v1/chat/plugins/example`}</CodeBlock>
      </section>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">sign-in or key, both work</h2>
        <p className="leading-relaxed">
          Servers with OAuth open a sign-in window and refresh their own tokens. Servers that take a key get it as{" "}
          <code>Authorization: Bearer</code>: c0mpute checks it against the server first, keeps it encrypted and never
          shows it again. The catalog has CoinPay, Mercury, ThreatCrush, Microsoft Learn, pit.world web search and more, one click each.
        </p>
      </section>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">safe by default</h2>
        <ul className="list-disc pl-6 space-y-1 leading-relaxed">
          <li>Anything that can change things stops at an approve / deny card. Only the signed approval for that exact call runs it.</li>
          <li>Money plugins run only read-only calls on their own; every payment or order shows its amount first.</li>
          <li>Only public https servers; no private or local addresses.</li>
          <li>A plugin that keeps failing switches itself off and tells you why.</li>
        </ul>
      </section>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">for developers</h2>
        <p className="leading-relaxed">
          Prefer your own loop? Send OpenAI-style <code>tools</code> to <code>/v1/chat/completions</code> and get{" "}
          <code>tool_calls</code> back from Qwen3, Devstral, Llama 3.x, Hermes 3 and other open models. See{" "}
          <Link href="/docs">the docs</Link>.
        </p>
      </section>

      <section className="space-y-2">
        <h2 className="text-lg font-bold accent">pay as you go</h2>
        <p className="leading-relaxed">
          Plugins are free to add. Each model step is billed like any chat reply, from a balance you top up with any coin
          or PayPal through CoinPay, from $5. No subscription.
        </p>
        <p>
          <a href={SIGN_IN} className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent">
            sign in and open chat
          </a>
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/">what c0mpute is</Link> · <Link href="/docs">API docs</Link> · <Link href="/checkout">add credit</Link>
      </p>
    </div>
  );
}
