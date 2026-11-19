import { DashboardTabs } from "@/components/dashboard-tabs";
import { EarningsDashboard } from "./earnings-dashboard";

export const metadata = {
  title: "my nodes: earnings vs spend — c0mpute",
  description: "What your c0mpute nodes earned against what they cost to run, by hour, day or month.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default function NodeEarningsPage() {
  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">my nodes</h1>
        <p className="comment">// what your nodes earned against what they cost to run</p>
        <DashboardTabs active="/dashboard/nodes" />
      </header>
      <EarningsDashboard />
    </div>
  );
}
