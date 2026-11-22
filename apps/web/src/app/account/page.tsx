import { AccountApp } from "./account-app";

export const metadata = {
  title: "account — c0mpute",
  description: "Your c0mpute account: CoinPay identity, passkeys, API keys, linked nodes, earnings, payouts and balance.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default async function AccountPage({ searchParams }: { searchParams: Promise<{ secure?: string; connected?: string; next?: string }> }) {
  const sp = await searchParams;
  return (
    <div className="max-w-4xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">account</h1>
        <p className="comment">// one identity for chat, nodes, fleet, the CLI, the API and MCP</p>
      </header>
      <AccountApp secure={sp.secure === "1"} connected={sp.connected ?? null} />
    </div>
  );
}
