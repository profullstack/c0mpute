import { DashboardTabs } from "@/components/dashboard-tabs";
import { LeaderboardPrivacy } from "@/components/leaderboard-privacy";
import { ReferDashboard } from "./refer-dashboard";

export const metadata = {
  title: "refer — c0mpute",
  description: "Your referral link: a share of the house fee on the nodes you bring in and on what the people you refer spend.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default function ReferPage() {
  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">refer people and nodes</h1>
        <p className="comment">// share your link: a share of the house fee on referred nodes&apos; paid jobs and on what referred people spend</p>
        <DashboardTabs active="/dashboard/refer" />
      </header>
      <ReferDashboard />
      <LeaderboardPrivacy />
    </div>
  );
}
