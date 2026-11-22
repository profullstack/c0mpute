import Link from "next/link";
import { CodeBlock as Code } from "@/components/CodeBlock";

export const metadata = {
  title: "cli — c0mpute",
  description: "The c0mpute CLI: sign in with CoinPay or a passkey, link your node, and run a fleet of nodes from the terminal.",
  alternates: { canonical: "https://c0mpute.com/cli" },
};

export default function CliPage() {
  return (
    <div className="max-w-3xl mx-auto px-4 sm:px-6 py-16 space-y-10 text-sm leading-7">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">cli</h1>
        <p className="comment">// install, sign in, link your node, run your fleet</p>
      </header>

      <section className="space-y-3">
        <h2 className="text-lg accent">install</h2>
        <Code>{`curl -fsSL https://c0mpute.com/install.sh | sh`}</Code>
      </section>

      <section id="sign-in" className="space-y-3">
        <h2 className="text-lg accent">sign in with CoinPay</h2>
        <p>
          <code>c0mpute login</code> uses the device flow: it prints a short code and a link; open it anywhere, sign in with CoinPay, a passkey or a
          password, and approve. The terminal gets an API key, saved to <code>~/.c0mpute/chat.json</code> (mode 0600). The TUI, <code>c0mpute chat</code> and every other command use the same key.
        </p>
        <Code>
{`c0mpute login        # approve the code at c0mpute.com/cli/approve
c0mpute whoami       # account, CoinPay DID, linked nodes
c0mpute logout`}
        </Code>
        <p>
          Prefer CoinPay if you run nodes: payouts go to your CoinPay wallet. Manage sign-in methods and keys on <Link href="/account">your account</Link>.
          For CI, set <code>C0MPUTE_API_KEY</code> instead.
        </p>
      </section>

      <section id="link-node" className="space-y-3">
        <h2 className="text-lg accent">link your node</h2>
        <p>
          <code>c0mpute node link</code> asks c0mpute.com for a one-shot challenge, signs it with the node&apos;s identity key and sends it back. The node is then
          verified to your account: it shows in your fleet, earns to your account and takes fleet commands, which it checks are signed by c0mpute.com
          for your account before running them.
        </p>
        <Code>
{`c0mpute node link [--label NAME] [--tags a,b]
c0mpute node link --token c0e_...      # headless: an enrollment token instead of a login
c0mpute node unlink                    # this node leaves the account`}
        </Code>
      </section>

      <section id="fleet" className="space-y-3">
        <h2 className="text-lg accent">fleet</h2>
        <Code>
{`c0mpute fleet ls [--tag T] [--json]               # status, version, hardware, jobs, earnings
c0mpute fleet show <node> [--json]
c0mpute fleet pause|resume <node...|--tag T|--all>   # stop/start bidding on new jobs
c0mpute fleet update  <node...|--tag T|--all>       # install the latest release
c0mpute fleet restart <node...|--tag T|--all>
c0mpute fleet tag <node...|--tag T> --set <tags>    # also: untag, set-tags
c0mpute fleet rename <node> <name>
c0mpute fleet unlink <node...>
c0mpute fleet earnings [--json]                     # per node: 24h, 30d, all
c0mpute fleet audit [--json]
c0mpute fleet token create --name rack-a --expires 7d [--tags rack-a] [--max-uses 50]
c0mpute fleet token ls | revoke <id|name>`}
        </Code>
        <p>
          Same API for scripts: <code>GET /api/v1/fleet</code>, <code>POST /api/v1/fleet/actions</code>, <code>/api/v1/fleet/tokens</code>,{" "}
          <code>/api/v1/fleet/audit</code>. MCP: <code>fleet_list</code>, <code>fleet_action</code>, <code>fleet_earnings</code>. See <Link href="/docs#fleet">docs</Link>.
        </p>
      </section>
    </div>
  );
}
