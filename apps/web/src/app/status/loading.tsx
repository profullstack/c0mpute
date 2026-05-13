import DashboardShell from "@/components/dashboard-shell";

export default function StatusLoading() {
  return (
    <DashboardShell>
      <section className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        {Array.from({ length: 4 }).map((_, i) => (
          <div
            key={i}
            className="rounded-2xl border border-[var(--color-rule)] bg-[var(--color-card)] p-5"
          >
            <div className="h-3 w-20 animate-pulse rounded bg-[var(--color-rule)]" />
            <div className="mt-4 h-8 w-24 animate-pulse rounded bg-[var(--color-line)]" />
            <div className="mt-3 h-3 w-32 animate-pulse rounded bg-[var(--color-rule)]" />
          </div>
        ))}
      </section>

      <section className="grid gap-6 xl:grid-cols-2">
        {Array.from({ length: 2 }).map((_, i) => (
          <div
            key={i}
            className="rounded-2xl border border-[var(--color-rule)] bg-[var(--color-card)] p-6"
          >
            <div className="h-4 w-32 animate-pulse rounded bg-[var(--color-rule)]" />
            <div className="mt-4 space-y-3">
              {Array.from({ length: 4 }).map((_, j) => (
                <div
                  key={j}
                  className="h-4 w-full animate-pulse rounded bg-[var(--color-line)]"
                />
              ))}
            </div>
          </div>
        ))}
      </section>
    </DashboardShell>
  );
}
