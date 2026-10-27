"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";

const LINKS: { href: string; label: string; accent?: boolean }[] = [
  { href: "/getting-started", label: "getting-started" },
  { href: "/protocol", label: "protocol" },
  { href: "/providers", label: "run a node", accent: true },
  { href: "/market", label: "market" },
  { href: "/nodes", label: "nodes" },
  { href: "/agents", label: "agents" },
  { href: "/plugins", label: "plugins" },
  { href: "/docs", label: "docs" },
  { href: "/status", label: "status" },
];

// One row on desktop; below lg the links fold into a <details> menu, which works
// without JS. The effect only closes it again after a client-side navigation.
export function SiteNav() {
  const pathname = usePathname();
  const menu = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    if (menu.current) menu.current.open = false;
  }, [pathname]);

  const linkClass = (accent?: boolean) =>
    `!border-0 whitespace-nowrap hover:text-[var(--color-accent)] ${accent ? "accent" : ""}`;

  return (
    <nav className="max-w-5xl mx-auto px-6 py-4 flex items-center justify-between gap-6 text-sm">
      <Link
        href="/"
        className="!border-0 shrink-0 whitespace-nowrap font-bold text-[var(--color-fg)] hover:text-[var(--color-accent)]"
      >
        <span className="accent">$</span> c0mpute
      </Link>

      <div className="hidden lg:flex items-center gap-x-5 text-[var(--color-dim)]">
        {LINKS.map((l) => (
          <Link key={l.href} href={l.href} className={linkClass(l.accent)}>
            {l.label}
          </Link>
        ))}
      </div>

      <details ref={menu} className="lg:hidden relative group">
        <summary className="list-none [&::-webkit-details-marker]:hidden cursor-pointer select-none px-3 py-1 border border-[var(--color-rule)] rounded text-[var(--color-dim)] hover:text-[var(--color-accent)] group-open:text-[var(--color-accent)]">
          <span className="group-open:hidden">menu</span>
          <span className="hidden group-open:inline">close</span>
        </summary>
        <div className="absolute right-0 top-full mt-2 z-50 min-w-48 flex flex-col py-2 bg-[var(--color-card)] border border-[var(--color-rule)] rounded shadow-lg text-[var(--color-dim)]">
          {LINKS.map((l) => (
            <Link key={l.href} href={l.href} className={`${linkClass(l.accent)} px-4 py-2`}>
              {l.label}
            </Link>
          ))}
        </div>
      </details>
    </nav>
  );
}
