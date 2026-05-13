export default function DashboardShell({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <div className="max-w-5xl mx-auto px-4 py-10 sm:px-6 lg:px-8 space-y-8">
      <header className="space-y-2">
        <p className="comment text-xs">// public network status</p>
        <h1 className="text-2xl font-bold accent tracking-tight">network status</h1>
        <p className="text-sm text-[var(--color-dim)] leading-relaxed max-w-2xl">
          Live snapshot of workers, jobs, and capabilities on the c0mpute
          decentralized compute network. All values are aggregates across the full
          network. No individual worker, job, or customer data is displayed.
        </p>
      </header>
      {children}
    </div>
  );
}
