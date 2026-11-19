import { DashboardTabs } from "@/components/dashboard-tabs";
import { DiskDashboard } from "./disk-dashboard";

export const metadata = {
  title: "my hosting: sites, apps, disk earnings and spend — c0mpute",
  description: "Your hosted sites and what they cost, and what your nodes earn holding other people's sites, by hour, day or month.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default function DiskPage() {
  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-3">
        <h1 className="text-2xl font-bold accent">my hosting</h1>
        <p className="comment">// your sites and apps and what they cost; what your nodes earn hosting everyone else&apos;s</p>
        <DashboardTabs active="/dashboard/disk" />
      </header>
      <DiskDashboard />
    </div>
  );
}
