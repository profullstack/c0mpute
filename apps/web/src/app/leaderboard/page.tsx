import Link from "next/link";
import DashboardShell from "@/components/dashboard-shell";
import OverviewGrid from "@/components/overview-grid";
import type { OverviewCard } from "@/components/overview-grid";
import ResourceTable from "@/components/resource-table";
import { CodeBlock } from "@/components/CodeBlock";
import { getLeaderboard } from "@/lib/leaderboard/server";
import { WINDOW_KEYS, type Board, type Window } from "@/lib/leaderboard/build";
import { usd } from "@/lib/nodes";
import { pageOg } from "@/lib/og";

const DESCRIPTION =
  "The c0mpute network's top performers, anonymous: nodes by jobs served, operators by real recorded earnings, referrers, users, API models and agents, over 24h, 7 days, 30 days and all time.";

export const metadata = {
  title: "leaderboard — c0mpute",
  description: DESCRIPTION,
  alternates: { canonical: "https://c0mpute.com/leaderboard" },
  ...pageOg("c0mpute leaderboard", DESCRIPTION, "/leaderboard"),
};

// Rendered per request from a payload rebuilt at most every 5 minutes
// (lib/leaderboard/server.ts): a page view never runs a query.
export const dynamic = "force-dynamic";

const WINDOW_LABEL: Record<Window, string> = { "24h": "24h", "7d": "7 days", "30d": "30 days", all: "all time" };
const SECTIONS = [
  ["nodes", "top nodes"],
  ["earners", "top earners"],
  ["affiliates", "top affiliates"],
  ["users", "top users"],
  ["api", "API"],
  ["agents", "agents"],
] as const;

const n = (v: number) => v.toLocaleString("en-US");
const empty = (b: Board<unknown>, none: string) => b.withheld ?? none;

export default async function LeaderboardPage({ searchParams }: { searchParams: Promise<{ w?: string }> }) {
  const { w: raw } = await searchParams;
  const w: Window = (WINDOW_KEYS as readonly string[]).includes(raw ?? "") ? (raw as Window) : "7d";
  const lb = await getLeaderboard();

  if (!lb) {
    return (
      <DashboardShell eyebrow="// public, anonymous" title="leaderboard" intro="The network's top performers.">
        <p className="text-sm text-[var(--color-dim)]">The leaderboard is rebuilding. Try again in a minute.</p>
      </DashboardShell>
    );
  }

  const b = lb.boards[w];
  const h = lb.headline;
  const cards: OverviewCard[] = [
    { label: "Workers online", value: n(h.workers_online), note: "right now" },
    { label: "Jobs served", value: n(h.jobs_served[w]), note: WINDOW_LABEL[w] },
    {
      label: "Paid to operators",
      value: h.paid_to_operators_usd ? usd(h.paid_to_operators_usd[w]) : "—",
      note: `${WINDOW_LABEL[w]} · recorded, not estimated`,
    },
    { label: "Active users", value: n(h.active_users[w]), note: `${WINDOW_LABEL[w]} · paying or signed in` },
  ];

  return (
    <DashboardShell
      eyebrow="// public, anonymous"
      title="leaderboard"
      intro={
        <>
          The network&apos;s top performers: nodes, operators, referrers, users, API and agents. Nobody is
          named. Nodes show their public id and the nickname their operator chose, as on{" "}
          <Link href="/nodes">/nodes</Link>; everyone else is an anon handle that can&apos;t be traced back.
          Money is real recorded pay, never an estimate.
        </>
      }
    >
      <nav className="flex flex-wrap items-center gap-2 text-sm" aria-label="time window">
        {WINDOW_KEYS.map((k) => (
          <Link
            key={k}
            href={k === "7d" ? "/leaderboard" : `/leaderboard?w=${k}`}
            aria-current={k === w ? "page" : undefined}
            className={`!border px-3 py-1 ${k === w ? "border-[var(--color-accent)] accent" : "border-[var(--color-rule)] text-[var(--color-dim)]"}`}
          >
            {WINDOW_LABEL[k]}
          </Link>
        ))}
      </nav>

      <OverviewGrid cards={cards} />

      <nav className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-[var(--color-dim)]" aria-label="boards">
        {SECTIONS.map(([id, label]) => (
          <a key={id} href={`#${id}`} className="hover:text-[var(--color-accent)]">
            {label}
          </a>
        ))}
      </nav>

      <div id="nodes" className="scroll-mt-24">
        <ResourceTable
          title={`[ top nodes · ${WINDOW_LABEL[w]} ]`}
          description="Ranked by jobs served, then bench score. Active days: days with at least one completed job. Online: on the board right now."
          columns={[
            { key: "rank", label: "#" },
            { key: "node", label: "Node" },
            { key: "operator", label: "Operator" },
            { key: "class", label: "Class" },
            { key: "jobs", label: "Jobs" },
            { key: "busy", label: "Busy" },
            { key: "days", label: "Active days" },
            { key: "bench", label: "Bench" },
            { key: "online", label: "Online" },
          ]}
          rows={b.nodes.rows.map((r) => ({
            id: r.node,
            rank: r.rank,
            node: r.node,
            operator: r.operator,
            class: r.class,
            jobs: n(r.jobs),
            busy: `${r.busy_hours}h`,
            days: r.active_days,
            bench: r.bench_score === null ? null : n(r.bench_score),
            online: r.online ? "yes" : "no",
          }))}
          emptyMessage={empty(b.nodes, "No node served a job in this window.")}
        />
      </div>

      <div id="earners" className="scroll-mt-24">
        <ResourceTable
          title={`[ top earners · ${WINDOW_LABEL[w]} ]`}
          description={`Operators by recorded earnings: what their nodes actually kept, after the ${lb.privacy.market_fee_pct}% fee. c0mpute auction jobs plus settled Infernet inference, each job counted once. An operator is their nickname (c0mpute name), else the node id, else an anon handle.`}
          columns={[
            { key: "rank", label: "#" },
            { key: "operator", label: "Operator" },
            { key: "nodes", label: "Nodes" },
            { key: "jobs", label: "Paid jobs" },
            { key: "earned", label: "Earned" },
          ]}
          rows={b.earners.rows.map((r) => ({ id: r.operator, rank: r.rank, operator: r.operator, nodes: r.nodes, jobs: n(r.jobs), earned: usd(r.earned_usd) }))}
          emptyMessage={empty(b.earners, "No recorded earnings in this window yet.")}
        />
      </div>

      <div id="affiliates" className="scroll-mt-24 space-y-4">
        <ResourceTable
          title={`[ top affiliates · ${WINDOW_LABEL[w]} ]`}
          description="Referral commissions earned (a share of the house fee on the paid jobs of nodes they referred, never the operator's share) and nodes referred in this window."
          columns={[
            { key: "rank", label: "#" },
            { key: "affiliate", label: "Affiliate" },
            { key: "nodes", label: "Nodes referred" },
            { key: "commissions", label: "Commissions" },
          ]}
          rows={b.affiliates.rows.map((r) => ({ id: r.affiliate, rank: r.rank, affiliate: r.affiliate, nodes: r.nodes_referred, commissions: usd(r.commissions_usd) }))}
          emptyMessage={empty(b.affiliates, "No referral activity in this window yet.")}
        />
        {lb.referral_handles.length > 0 && (
          <ResourceTable
            title="[ referral handles · online now ]"
            description="Public C0MPUTE_REF handles and the nodes online now that installed with them, as on /nodes."
            columns={[
              { key: "rank", label: "#" },
              { key: "handle", label: "Handle" },
              { key: "nodes", label: "Nodes online" },
            ]}
            rows={lb.referral_handles.map((r) => ({ id: r.handle, ...r }))}
            emptyMessage="No referral handles online."
          />
        )}
      </div>

      <div id="users" className="scroll-mt-24">
        <ResourceTable
          title={`[ top users · ${WINDOW_LABEL[w]} ]`}
          description="Accounts by what they spent on compute (shown as a range, never exact), then by calls to c0mpute chat, API and MCP. House accounts are not ranked. Counts are rounded."
          columns={[
            { key: "rank", label: "#" },
            { key: "user", label: "User" },
            { key: "spend", label: "Spent" },
            { key: "billed", label: "Paid jobs" },
            { key: "calls", label: "Calls" },
          ]}
          rows={b.users.rows.map((r) => ({ id: r.user, rank: r.rank, user: r.user, spend: r.spend, billed: n(r.billed_jobs), calls: n(r.calls) }))}
          emptyMessage={empty(b.users, "No user activity in this window yet.")}
        />
      </div>

      <div id="api" className="scroll-mt-24 space-y-4">
        <OverviewGrid
          cards={[
            { label: "API calls", value: n(b.api.totals.api_calls), note: "c0mpute.com/v1 with a key" },
            { label: "Chat turns", value: n(b.api.totals.chat_calls), note: "c0mpute.com/chat" },
            { label: "Inference jobs", value: n(b.api.totals.served_jobs), note: "settled on the ledger" },
            { label: "Output tokens", value: n(b.api.totals.output_tokens), note: WINDOW_LABEL[w] },
          ]}
        />
        <div className="grid gap-6 xl:grid-cols-2">
          <ResourceTable
            title={`[ top models · ${WINDOW_LABEL[w]} ]`}
            description="Inference jobs served per model, with output tokens. A model fewer than 3 people used is counted under other."
            columns={[
              { key: "label", label: "Model" },
              { key: "calls", label: "Jobs" },
              { key: "tokens", label: "Output tokens" },
              { key: "users", label: "Users" },
            ]}
            rows={b.api.models.rows.map((r) => ({ id: r.label, label: r.label, calls: n(r.calls), tokens: r.output_tokens === undefined ? null : n(r.output_tokens), users: r.users }))}
            emptyMessage={empty(b.api.models, "No inference in this window.")}
          />
          <ResourceTable
            title={`[ top API callers · ${WINDOW_LABEL[w]} ]`}
            description="Accounts by calls to the OpenAI-compatible API (c0mpute.com/v1) with a c0k_ key. Counts are rounded."
            columns={[
              { key: "rank", label: "#" },
              { key: "user", label: "Caller" },
              { key: "calls", label: "Calls" },
            ]}
            rows={b.api.callers.rows.map((r) => ({ id: r.user, rank: r.rank, user: r.user, calls: n(r.calls) }))}
            emptyMessage={empty(b.api.callers, "No API calls in this window yet.")}
          />
        </div>
      </div>

      <div id="agents" className="scroll-mt-24 space-y-4">
        <div className="grid gap-6 xl:grid-cols-2">
          <ResourceTable
            title={`[ MCP tools · ${WINDOW_LABEL[w]} ]`}
            description={`Agent calls to the c0mpute MCP server (c0mpute.com/api/mcp) by tool: ${n(b.agents.totals.mcp_calls)} in this window. A tool fewer than 3 agents used is counted under other.`}
            columns={[
              { key: "label", label: "Tool" },
              { key: "calls", label: "Calls" },
              { key: "users", label: "Agents" },
            ]}
            rows={b.agents.tools.rows.map((r) => ({ id: r.label, label: r.label, calls: n(r.calls), users: r.users }))}
            emptyMessage={empty(b.agents.tools, "No MCP calls in this window yet.")}
          />
          <ResourceTable
            title={`[ top agents · ${WINDOW_LABEL[w]} ]`}
            description="Accounts by MCP tool calls (Claude, ChatGPT and other agents signed in to c0mpute). Counts are rounded."
            columns={[
              { key: "rank", label: "#" },
              { key: "user", label: "Agent account" },
              { key: "calls", label: "Calls" },
            ]}
            rows={b.agents.agents.rows.map((r) => ({ id: r.user, rank: r.rank, user: r.user, calls: n(r.calls) }))}
            emptyMessage={empty(b.agents.agents, "No agent activity in this window yet.")}
          />
        </div>
      </div>

      <section className="space-y-3 rounded-2xl border border-[var(--color-rule)] bg-[var(--color-card)] p-5">
        <h2 className="text-base font-semibold text-[var(--color-accent)]">[ get on the board ]</h2>
        <p className="text-sm text-[var(--color-dim)]">
          Run a node: one line installs it and starts earning on the jobs it wins. Name it so the board shows
          your nickname instead of a node id.
        </p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh -s -- --name <your-handle>`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Bring other nodes: share <span className="accent">https://c0mpute.com/r/&lt;handle&gt;</span>. Nodes that
          install from it show your handle, and a paid referral code from{" "}
          <Link href="/dashboard/refer">your dashboard</Link> earns a share of the house fee on every paid job they
          serve. <Link href="/providers">How running a node works</Link> ·{" "}
          <Link href="/estimates">what your hardware could earn</Link>
        </p>
      </section>

      <footer className="text-xs text-[var(--color-dim)] space-y-2 pt-6 rule">
        <p>
          How it stays anonymous: no email, DID, wallet, IP, account id, API key, prompt or job content is ever
          shown. People appear as anon handles made with a keyed hash that only this server can compute, so a
          handle can&apos;t be reversed or matched to an account. A board with fewer than {lb.privacy.min_participants}{" "}
          people in a window is held back, a model or tool fewer than {lb.privacy.min_participants} people used is
          counted under other, customer spend is shown as a range and counts are rounded. Signed in? You can{" "}
          <Link href="/dashboard/nodes">hide yourself from the leaderboard</Link>.
        </p>
        <p>
          Sources: the c0mpute job ledger copied from the status aggregators
          {lb.history.node_jobs_from ? ` (since ${lb.history.node_jobs_from})` : ""}, the Infernet earnings and
          billing ledgers, and c0mpute.com call counters
          {lb.history.usage_from ? ` (since ${lb.history.usage_from})` : " (new: they start filling now)"}. Rebuilt every 5
          minutes; generated {lb.generated_at}. JSON: <a href="/api/leaderboard">/api/leaderboard</a>.
        </p>
        <p>
          &rarr; <Link href="/status">network status</Link> · <Link href="/nodes">nodes online</Link> ·{" "}
          <Link href="/benchmark">benchmarks</Link>
        </p>
      </footer>
    </DashboardShell>
  );
}
