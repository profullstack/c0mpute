import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { getNodesPayload } from "@/lib/nodes";
import { MARKET_FEE_PCT, OPERATOR_SHARE_PCT, splitPrice } from "@/lib/market";

export const metadata = {
  title: "market — c0mpute",
  description: `Rent GPU and CPU time from the c0mpute nodes online right now. The node operator keeps ${OPERATOR_SHARE_PCT}% of every job; c0mpute takes a ${MARKET_FEE_PCT}% market fee.`,
  alternates: { canonical: "https://c0mpute.com/market" },
};

// Live supply, not a build-time snapshot.
export const dynamic = "force-dynamic";

const EXAMPLES = [1, 5, 20];

function ago(s: number): string {
  if (s < 90) return `${s}s ago`;
  return `${Math.round(s / 60)}m ago`;
}

export default async function MarketPage() {
  const p = await getNodesPayload();
  const gpus = p.nodes.filter((n) => n.class.startsWith("gpu"));
  const cpus = p.nodes.length - gpus.length;

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">market</h1>
        <p className="comment">
          // rent compute from the nodes online now · operators keep {OPERATOR_SHARE_PCT}%
        </p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ the split ]</h2>
        <p>
          Every job bought through the market pays the node operator{" "}
          <strong className="text-[var(--color-fg)]">{OPERATOR_SHARE_PCT}%</strong> of
          the price. c0mpute keeps a{" "}
          <strong className="text-[var(--color-fg)]">{MARKET_FEE_PCT}% market fee</strong>{" "}
          for matching, billing, settlement and support. Payment-processor fees
          come out of the payment, not out of the operator&apos;s share on top.
        </p>
        <table className="w-full text-left border-collapse text-xs">
          <thead className="text-[var(--color-dim)]">
            <tr>
              <th className="py-2 pr-6">job price</th>
              <th className="py-2 pr-6">operator gets</th>
              <th className="py-2">c0mpute fee</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-[var(--color-rule)]">
            {EXAMPLES.map((usd) => {
              const s = splitPrice(usd);
              return (
                <tr key={usd}>
                  <td className="py-2 pr-6">${usd.toFixed(2)}</td>
                  <td className="py-2 pr-6 accent">${s.operator.toFixed(2)}</td>
                  <td className="py-2 text-[var(--color-dim)]">${s.fee.toFixed(2)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          The same {MARKET_FEE_PCT}% is the house fee on paid{" "}
          <a href="https://infernetprotocol.com" target="_blank" rel="noopener noreferrer">Infernet</a>{" "}
          inference jobs, which run on these nodes. Two peers dealing directly
          over the open protocol, without the market, pay no protocol fee. →{" "}
          <Link href="/pricing">pricing</Link>
        </p>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-semibold">[ GPUs online now ]</h2>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          {gpus.length} GPU {gpus.length === 1 ? "node" : "nodes"} and {cpus} CPU{" "}
          {cpus === 1 ? "node" : "nodes"} online, read from the signed adverts
          each worker gossips every minute.
          {p.source === "unavailable" ? " The aggregator is not answering right now, so the list is empty." : ""}
        </p>
        {gpus.length === 0 ? (
          <p className="text-sm text-[var(--color-dim)]">No GPU nodes online at the moment.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left border-collapse text-xs">
              <thead className="text-[var(--color-dim)]">
                <tr>
                  <th className="py-2 pr-3">node</th>
                  <th className="py-2 pr-3">class</th>
                  <th className="py-2 pr-3">bench</th>
                  <th className="py-2 pr-3">version</th>
                  <th className="py-2">seen</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--color-rule)]">
                {gpus.map((n, i) => (
                  <tr key={`${n.id}-${i}`}>
                    <td className="py-2 pr-3 accent font-mono">{n.id}</td>
                    <td className="py-2 pr-3">{n.class}</td>
                    <td className="py-2 pr-3">{n.bench_score?.toLocaleString() ?? "-"}</td>
                    <td className="py-2 pr-3 text-[var(--color-dim)]">{n.version ?? "-"}</td>
                    <td className="py-2 text-[var(--color-dim)]">{ago(n.last_seen_s)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/nodes">every node</Link> · <a href="/api/nodes">/api/nodes</a> (JSON)
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ rent ]</h2>
        <p>
          <strong className="text-[var(--color-fg)]">GPU inference.</strong> Paid
          model inference runs through{" "}
          <a href="https://infernetprotocol.com" target="_blank" rel="noopener noreferrer">Infernet</a>{" "}
          on these nodes, billed through{" "}
          <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">CoinPay</a>{" "}
          in crypto or PayPal.
        </p>
        <p>
          <strong className="text-[var(--color-fg)]">Batch jobs.</strong> Submit
          from the CLI with a price cap; nodes quote under it and the job pays
          out when the result validates.
        </p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh
$ c0mpute transcode submit input.mov --preset hls --max-price 1.25`}</CodeBlock>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ sell your GPU ]</h2>
        <p>
          Install the node, set where your {OPERATOR_SHARE_PCT}% lands, and it
          shows up here as soon as it advertises.
        </p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh
$ infernet payout set USDC 0xYourAddress --network polygon`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/providers">running a node</Link>
        </p>
      </section>
    </div>
  );
}
