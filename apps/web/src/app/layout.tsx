import type { Metadata, Viewport } from "next";
import Link from "next/link";
import "./globals.css";
import Script from "next/script";
import { Footer } from "@profullstack/footer/react";
import { WebMcpProvider } from "@/components/webmcp-provider";
import { PitFooter } from "@/components/pit-footer";

const SITE = "https://c0mpute.com";
const DESCRIPTION =
  "The open compute network. Run jobs anywhere, sell unused compute, pay only for verified work. CPUs, GPUs, storage and bandwidth from PCs to data centers, through one open protocol — for humans and AI agents.";

// Static pages re-render hourly so the pit.world network footer (fetched server-side
// from pit.world/snippets) and the @profullstack/footer webring row (rendered from the
// package's @latest template) pick up changes without a redeploy. Without this the
// prerendered pages are baked at build (s-maxage=31536000) and only a redeploy
// would change the footer.
export const revalidate = 3600;

export const metadata: Metadata = {
  title: "c0mpute — the open compute network",
  description: DESCRIPTION,
  manifest: "/manifest.json",
  appleWebApp: { capable: true, statusBarStyle: "default", title: "c0mpute" },
  openGraph: {
    type: "website",
    siteName: "c0mpute",
    title: "c0mpute — the open compute network",
    description: DESCRIPTION,
    url: SITE,
    images: [{ url: `${SITE}/og-image.png`, width: 1200, height: 630, alt: "c0mpute" }],
  },
  twitter: {
    card: "summary_large_image",
    title: "c0mpute — the open compute network",
    description: DESCRIPTION,
    images: [`${SITE}/og-image.png`],
  },
  alternates: { canonical: SITE },
};

export const viewport: Viewport = {
  themeColor: "#0a0a0b",
  width: "device-width",
  initialScale: 1,
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className="min-h-screen flex flex-col">
        <WebMcpProvider />
        <header className="border-b border-[var(--color-rule)]">
          <nav className="max-w-3xl mx-auto px-6 py-4 flex items-center justify-between text-sm">
            <Link
              href="/"
              className="!border-0 font-bold text-[var(--color-fg)] hover:text-[var(--color-accent)]"
            >
              <span className="accent">$</span> c0mpute
            </Link>
            <div className="flex flex-wrap justify-end gap-x-5 gap-y-1 text-[var(--color-dim)]">
              <Link href="/getting-started" className="!border-0 hover:text-[var(--color-accent)]">getting-started</Link>
              <Link href="/protocol" className="!border-0 hover:text-[var(--color-accent)]">protocol</Link>
              <Link href="/providers" className="!border-0 accent hover:text-[var(--color-accent)]">run a node</Link>
              <Link href="/nodes" className="!border-0 hover:text-[var(--color-accent)]">nodes</Link>
              <Link href="/agents" className="!border-0 hover:text-[var(--color-accent)]">agents</Link>
              <Link href="/plugins" className="!border-0 hover:text-[var(--color-accent)]">plugins</Link>
              <Link href="/docs" className="!border-0 hover:text-[var(--color-accent)]">docs</Link>
              <Link href="/status" className="!border-0 hover:text-[var(--color-accent)]">status</Link>
            </div>
          </nav>
        </header>

        <main className="flex-1">{children}</main>

        <footer className="mt-16 border-t border-[var(--color-rule)]">
          <div className="max-w-3xl mx-auto px-6 py-5 flex flex-wrap items-center justify-center gap-x-5 gap-y-2 text-xs text-[var(--color-dim)] [&_a]:!border-0">
            <nav aria-label="Footer" className="flex flex-wrap justify-center gap-x-4 gap-y-1">
              {[
                ["blog", "/blog"],
                ["about", "/about"],
                ["contact", "/contact"],
                ["pricing", "/pricing"],
                ["terms", "/terms"],
                ["privacy", "/privacy"],
              ].map(([label, href]) => (
                <Link key={href} href={href} className="hover:text-[var(--color-accent)]">{label}</Link>
              ))}
              <a href="https://github.com/profullstack/c0mpute" className="hover:text-[var(--color-accent)]">github</a>
            </nav>
          </div>
        </footer>
        {/* The Profullstack webring row (server-rendered by @profullstack/footer), which
            also carries the copyright. The pit.world network footer stays last in <body>. */}
        <div className="text-xs text-[var(--color-dim)] [&_a]:!border-0">
          <Footer site="https://c0mpute.com/" tagline="MIT licensed" />
        </div>
              <Script data-site="130ff3f6-f531-4f4b-b732-73a3f0d072b1" src="https://crawlproof.com/stats.js" strategy="afterInteractive" />
              <script
                type="application/ld+json"
                dangerouslySetInnerHTML={{
                  __html: JSON.stringify([
                    {
                      "@context": "https://schema.org",
                      "@type": "Organization",
                      name: "c0mpute",
                      url: "https://c0mpute.com",
                      description: DESCRIPTION,
                      license: "https://opensource.org/licenses/MIT",
                      sameAs: ["https://github.com/profullstack/c0mpute"],
                    },
                    {
                      "@context": "https://schema.org",
                      "@type": "SoftwareApplication",
                      name: "c0mpute",
                      applicationCategory: "DeveloperApplication",
                      operatingSystem: "Linux, macOS",
                      downloadUrl: "https://c0mpute.com/install.sh",
                      softwareVersion: "0.2.0",
                      license: "https://opensource.org/licenses/MIT",
                      offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
                      description: DESCRIPTION,
                      url: "https://c0mpute.com",
                    },
                  ]),
                }}
              />
        <PitFooter />
      </body>
    </html>
  );
}
