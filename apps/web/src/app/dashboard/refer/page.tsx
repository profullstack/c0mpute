import { DashboardTabs } from "@/components/dashboard-tabs";
import { LeaderboardPrivacy } from "@/components/leaderboard-privacy";
import { ReferDashboard } from "./refer-dashboard";

export const metadata = {
  title: "refer a node — c0mpute",
  description: "Your node referral link and the share of the house fee you earn on the nodes you bring in.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default function ReferPage() {
  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">refer a node</h1>
        <p className="comment">// bring a node to the network, get a share of the house fee on its paid jobs</p>
        <DashboardTabs active="/dashboard/refer" />
      </header>
      <ReferDashboard />
      <LeaderboardPrivacy />
    </div>
  );
}
