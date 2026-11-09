import Link from "next/link";

export const metadata = {
  title: "apps: backends with 3x redundancy — c0mpute",
  description:
    "Run an API, MCP server or worker with a database on the c0mpute network instead of a VPS. Three copies, one primary, state snapshotted encrypted to the standbys, failover with no coordinator. git push c0mpute://<name> to deploy.",
  alternates: { canonical: "https://c0mpute.com/apps" },
};

const Code = ({ children }: { children: string }) => (
  <pre className="overflow-x-auto border border-[var(--color-border,#333)] p-4 text-xs leading-6">
    <code>{children}</code>
  </pre>
);

export default function AppsPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">apps: backends without a server</h1>
        <p className="comment">// a container image in, three copies on the network out. one runs, two stand by, nobody coordinates</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what it is ]</h2>
        <p>
          Static sites already live on c0mpute with three self-healing copies. Apps do the same for code that runs:
          an API, an MCP server, a bot, a worker with a SQLite database. Anything in a container that answers HTTP on
          one port. Every gateway serves it at <code>/a/&lt;app_id&gt;/</code> and on your own domain. The nodes
          that run it can sit behind NAT, because gateways reach them over libp2p.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ deploy ]</h2>
        <Code>{`curl -fsSL https://c0mpute.com/install.sh | sh        # c0mpute 0.2.99+

c0mpute apps deploy ./api --name api --port 8080 --state /data
building ./api ...
image c0mpute-app/api:1762... (38 MiB gzipped) published as site 4be1...
app api is 1dc2367caf3f51b0c249
  holders 3/3, primary ...8f2a91c4 (healthy), image 3/3 copies
up: /a/1dc2367caf3f51b0c249/ on any gateway

c0mpute apps secrets set api DATABASE_KEY=...   # sealed to each holder
c0mpute apps ls | status api | logs api | destroy api --yes`}</Code>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ or git push ]</h2>
        <Code>{`c0mpute git install
git remote add c0 c0mpute://api
git push c0 main          # repo kept by the network; deploys when c0mpute.toml has [app]
git clone c0mpute://<repo_id>    # works with the owner offline

# c0mpute.toml
[app]
name = "api"
port = 8080
health = "/healthz"
state = "/data"
domains = ["api.example.org"]`}</Code>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ for agents: MCP ]</h2>
        <Code>{`claude mcp add c0mpute-apps -- c0mpute apps mcp
# tools: apps_deploy, apps_list, apps_status, apps_logs, apps_secret_set, apps_destroy`}</Code>
        <p>
          The HTTP API is on every gateway: <code>GET /apps/v1/apps</code>, <code>GET /apps/v1/apps/&lt;id&gt;</code>,
          and <code>POST /apps/v1/records</code> for an owner-signed record from any client that can sign one.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ how it stays up ]</h2>
        <ul className="space-y-1 list-none">
          <li><span className="accent">placement</span>: every apps node gossips what it holds, and every node computes the same three holders by rendezvous hashing. No leader.</li>
          <li><span className="accent">primary</span>: one holder runs the container. Its claim is sticky and fenced by an epoch number.</li>
          <li><span className="accent">state</span>: every 30 s (configurable) the primary takes a crash-consistent copy of the state directory, encrypted under the app key in 1 MiB pieces that dedupe, and the standbys pull only what changed.</li>
          <li><span className="accent">failover</span>: when the primary goes quiet for 35 s, the standby with the newest snapshot promotes itself at the next epoch and restores. You lose at most one snapshot interval of writes.</li>
          <li><span className="accent">secrets</span>: sealed to each holder&apos;s own key, from your machine and then holder to holder, so you can go offline.</li>
        </ul>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ run apps for the network ]</h2>
        <Code>{`c0mpute worker start --roles ...,apps      # opt-in; needs docker

# config.toml
[apps]
max_apps = 4
max_memory_mb = 2048`}</Code>
        <p>
          Containers run with all capabilities dropped except a few basics, no-new-privileges, memory, CPU and pid
          limits, and a loopback-only port. <Link href="/nodes">See who is online.</Link>
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ honest notes ]</h2>
        <ul className="space-y-1 list-none">
          <li><span className="accent">one writer</span>: a stateful app has a single primary. This is a replicated single-node database, not a distributed SQL cluster (that comes next).</li>
          <li><span className="accent">trust</span>: the node running your container can see its memory and disk, like any VPS host. Snapshots at rest on the standbys are encrypted.</li>
          <li><span className="accent">http only</span>: requests up to 700 KiB, responses up to 5 MiB, no websockets yet.</li>
          <li><span className="accent">early</span>: free and best-effort while few apps nodes are online; pricing will reuse hosting settlement.</li>
        </ul>
        <p>
          Docs: <a href="https://github.com/profullstack/c0mpute/blob/master/docs/apps.md">docs/apps.md</a>
        </p>
      </section>
    </div>
  );
}
