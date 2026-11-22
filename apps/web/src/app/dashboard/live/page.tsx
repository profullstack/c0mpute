import { DashboardTabs } from "@/components/dashboard-tabs";
import { LiveDashboard } from "./live-dashboard";

export const metadata = {
  title: "live streams — c0mpute",
  description: "Your live streams on c0mpute Live: stream keys, watch links and what relays carried.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default function LiveDashboardPage() {
  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">live streams</h1>
        <p className="comment">// push RTMP from OBS or PairUX; c0mpute nodes relay it and anyone watches with a link</p>
        <DashboardTabs active="/dashboard/live" />
      </header>
      <LiveDashboard />
    </div>
  );
}
