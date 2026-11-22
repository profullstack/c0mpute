"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";

type NavLink = { href: string; label: string; desc: string };
type NavGroup = { id: string; label: string; links: NavLink[] };

// Every page the site links to, grouped by what a visitor came to do. Add new
// pages to a group here; the desktop menus and the mobile menu both read it.
const NAV_GROUPS: NavGroup[] = [
  {
    id: "use",
    label: "use",
    links: [
      { href: "/chat", label: "chat", desc: "open models on the network" },
      { href: "/gallery", label: "gallery", desc: "what people built on c0mpute" },
      { href: "/train", label: "train an AI agent", desc: "fine-tune on rented nodes" },
      { href: "/apps", label: "apps & hosting", desc: "sites and backends, 3 copies" },
      { href: "/market", label: "market", desc: "rent GPU and CPU time" },
      { href: "/dashboard", label: "my dashboard", desc: "balance, keys, jobs" },
      { href: "/dashboard/disk", label: "my hosting", desc: "your sites and apps" },
    ],
  },
  {
    id: "earn",
    label: "earn",
    links: [
      { href: "/providers", label: "run a node", desc: "sell your spare compute" },
      { href: "/estimates", label: "earnings estimates", desc: "what your hardware makes" },
      { href: "/dashboard/nodes", label: "node dashboard", desc: "your earnings vs spend" },
      { href: "/nodes", label: "nodes online", desc: "the live network" },
      { href: "/benchmark", label: "benchmarks", desc: "how fast nodes are" },
    ],
  },
  {
    id: "build",
    label: "build",
    links: [
      { href: "/getting-started", label: "getting started", desc: "install and first job" },
      { href: "/docs", label: "docs", desc: "CLI, API, hosting, jobs" },
      { href: "/agents", label: "agents & MCP", desc: "compute for AI agents" },
      { href: "/plugins", label: "plugins", desc: "add workloads to a node" },
      { href: "/protocol", label: "protocol", desc: "how the network works" },
      { href: "/temporal", label: "temporal policies", desc: "liquid networks" },
    ],
  },
  {
    id: "company",
    label: "company",
    links: [
      { href: "/about", label: "about", desc: "why c0mpute exists" },
      { href: "/pricing", label: "pricing", desc: "fees and free tier" },
      { href: "/status", label: "status", desc: "network health" },
      { href: "/blog", label: "blog", desc: "news and write-ups" },
      { href: "/zero-token", label: "zero token", desc: "no coin, ever" },
      { href: "/contact", label: "contact", desc: "issues and email" },
    ],
  },
];

const isActive = (pathname: string, href: string) =>
  href === "/dashboard" ? pathname === href : pathname === href || pathname.startsWith(href + "/");

// Grouped header. Desktop (lg+): four menus built on <details name="site-nav">,
// so only one is open at a time and they open and close with JS off; Gallery,
// dashboard and chat sit beside them as plain links. Below lg: one "menu"
// <details> that opens a full-screen grouped list. The effect only adds
// niceties: Escape closes and returns focus, a click outside closes, and a
// client-side navigation closes everything.
export function SiteNav() {
  const pathname = usePathname() || "/";
  const root = useRef<HTMLElement>(null);

  useEffect(() => {
    root.current?.querySelectorAll("details[open]").forEach((d) => ((d as HTMLDetailsElement).open = false));
  }, [pathname]);

  useEffect(() => {
    const el = root.current;
    if (!el) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const open = el.querySelector<HTMLDetailsElement>("details[open]");
      if (!open) return;
      open.open = false;
      open.querySelector<HTMLElement>("summary")?.focus();
    };
    const onClick = (e: MouseEvent) => {
      el.querySelectorAll<HTMLDetailsElement>("details.nav-menu[open]").forEach((d) => {
        if (!d.contains(e.target as Node)) d.open = false;
      });
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("click", onClick);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("click", onClick);
    };
  }, []);

  const linkCls = (href: string) =>
    `!border-0 hover:text-[var(--color-accent)] ${isActive(pathname, href) ? "text-[var(--color-accent)]" : ""}`;
  const current = (href: string) => (isActive(pathname, href) ? ("page" as const) : undefined);

  return (
    <nav ref={root} aria-label="Main" className="site-nav max-w-6xl mx-auto px-4 sm:px-6 h-14 flex items-center gap-4 text-sm">
      <Link
        href="/"
        className="!border-0 shrink-0 whitespace-nowrap font-bold text-[var(--color-fg)] hover:text-[var(--color-accent)]"
      >
        <span className="accent">$</span> c0mpute
      </Link>

      {/* desktop */}
      <ul className="hidden lg:flex items-center gap-1 ml-4 text-[var(--color-dim)]">
        {NAV_GROUPS.map((g) => {
          const here = g.links.some((l) => isActive(pathname, l.href));
          return (
            <li key={g.id} className="relative">
              <details name="site-nav" className="nav-menu group">
                <summary
                  className={`nav-summary list-none [&::-webkit-details-marker]:hidden cursor-pointer select-none px-3 py-1.5 rounded hover:text-[var(--color-accent)] group-open:text-[var(--color-accent)] group-open:bg-[var(--color-card)] ${here ? "text-[var(--color-fg)]" : ""}`}
                >
                  {g.label}
                  <span aria-hidden="true" className="ml-1 inline-block text-[10px] transition-transform group-open:rotate-180">
                    ▾
                  </span>
                </summary>
                <div className="nav-panel absolute left-0 top-full mt-2 z-50 w-[25rem] p-2 bg-[var(--color-card)] border border-[var(--color-rule)] rounded-lg shadow-2xl">
                  <ul className="grid gap-0.5">
                    {g.links.map((l) => (
                      <li key={l.href}>
                        <Link
                          href={l.href}
                          aria-current={current(l.href)}
                          className={`${linkCls(l.href)} grid grid-cols-[9.5rem_1fr] items-baseline gap-3 px-3 py-2 rounded hover:bg-[var(--color-panel)] focus-visible:bg-[var(--color-panel)]`}
                        >
                          <span className={isActive(pathname, l.href) ? "" : "text-[var(--color-fg)]"}>{l.label}</span>
                          <span className="text-xs text-[var(--color-dim)]">{l.desc}</span>
                        </Link>
                      </li>
                    ))}
                  </ul>
                </div>
              </details>
            </li>
          );
        })}
        <li>
          <Link href="/gallery" aria-current={current("/gallery")} className={`${linkCls("/gallery")} px-3 py-1.5 rounded inline-flex items-center gap-1.5`}>
            gallery
            <span className="text-[10px] uppercase tracking-wider px-1.5 rounded bg-[var(--color-accent-dim)] text-[var(--color-fg)]">new</span>
          </Link>
        </li>
      </ul>

      <div className="ml-auto flex items-center gap-3">
        <Link
          href="/dashboard"
          aria-current={current("/dashboard")}
          className={`${linkCls("/dashboard")} hidden lg:inline text-[var(--color-dim)] whitespace-nowrap`}
        >
          my dashboard
        </Link>
        <Link
          href="/chat"
          aria-current={current("/chat")}
          className="!border !border-[var(--color-accent)] px-3 py-1 rounded whitespace-nowrap text-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/40"
        >
          chat
        </Link>

        {/* mobile */}
        <details className="nav-menu nav-mobile lg:hidden group">
          <summary
            aria-label="menu"
            className="list-none [&::-webkit-details-marker]:hidden cursor-pointer select-none px-3 py-1 border border-[var(--color-rule)] rounded text-[var(--color-dim)] hover:text-[var(--color-accent)] group-open:text-[var(--color-accent)]"
          >
            <span className="group-open:hidden">menu</span>
            <span className="hidden group-open:inline">close</span>
          </summary>
          <div className="nav-sheet fixed inset-x-0 bottom-0 top-14 z-50 overflow-y-auto overscroll-contain bg-[var(--color-bg)] border-t border-[var(--color-rule)]">
            <div className="max-w-xl mx-auto px-4 py-6 space-y-8">
              <Link
                href="/gallery"
                className="!border block rounded-lg !border-[var(--color-accent-dim)] p-4 bg-[var(--color-card)]"
              >
                <span className="accent font-bold">gallery</span>
                <span className="block text-xs text-[var(--color-dim)] mt-1">what people built on c0mpute: apps, music, podcasts, models</span>
              </Link>
              {NAV_GROUPS.map((g) => (
                <section key={g.id} aria-labelledby={`nav-m-${g.id}`}>
                  <h2 id={`nav-m-${g.id}`} className="text-xs uppercase tracking-widest text-[var(--color-muted)] mb-2">
                    {g.label}
                  </h2>
                  <ul className="grid gap-px">
                    {g.links.map((l) => (
                      <li key={l.href}>
                        <Link
                          href={l.href}
                          aria-current={current(l.href)}
                          className={`${linkCls(l.href)} flex items-baseline justify-between gap-4 py-3 border-b border-[var(--color-line)]`}
                        >
                          <span className={isActive(pathname, l.href) ? "" : "text-[var(--color-fg)]"}>{l.label}</span>
                          <span className="text-xs text-[var(--color-dim)] text-right">{l.desc}</span>
                        </Link>
                      </li>
                    ))}
                  </ul>
                </section>
              ))}
            </div>
          </div>
        </details>
      </div>
    </nav>
  );
}
