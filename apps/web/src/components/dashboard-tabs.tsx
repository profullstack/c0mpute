import Link from "next/link";

const TABS = [
  { href: "/dashboard/disk", label: "hosting: sites, apps, disk" },
  { href: "/dashboard/nodes", label: "nodes: compute" },
  { href: "/dashboard/live", label: "live streams" },
  { href: "/dashboard/refer", label: "refer a node" },
] as const;

/** The parts of "my dashboard", one click apart. */
export function DashboardTabs({ active }: { active: (typeof TABS)[number]["href"] }) {
  return (
    <nav className="flex flex-wrap gap-2 text-sm" aria-label="dashboard">
      {TABS.map((t) => (
        <Link
          key={t.href}
          href={t.href}
          aria-current={active === t.href ? "page" : undefined}
          className={`!border px-3 py-1 ${active === t.href ? "border-[var(--color-accent)] accent" : "border-[var(--color-rule)] text-[var(--color-dim)]"}`}
        >
          {t.label}
        </Link>
      ))}
    </nav>
  );
}
