import type { Metadata, Viewport } from "next";
import Link from "next/link";
import "./globals.css";
import Script from "next/script";
import { FeedbackWidget } from "@profullstack/stack/feedback";

const SITE = "https://c0mpute.com";
const DESCRIPTION =
  "Decentralized compute marketplace. CLI-first. Three modules: transcode (FFmpeg), coinpay (DID + payments), infernet (AI inference).";

export const metadata: Metadata = {
  title: "c0mpute — decentralized compute network",
  description: DESCRIPTION,
  manifest: "/manifest.json",
  appleWebApp: { capable: true, statusBarStyle: "default", title: "c0mpute" },
  openGraph: {
    type: "website",
    siteName: "c0mpute",
    title: "c0mpute — decentralized compute network",
    description: DESCRIPTION,
    url: SITE,
    images: [{ url: `${SITE}/og-image.png`, width: 1200, height: 630, alt: "c0mpute" }],
  },
  twitter: {
    card: "summary_large_image",
    title: "c0mpute — decentralized compute network",
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
        <header className="border-b border-[var(--color-rule)]">
          <nav className="max-w-3xl mx-auto px-6 py-4 flex items-center justify-between text-sm">
            <Link
              href="/"
              className="!border-0 font-bold text-[var(--color-fg)] hover:text-[var(--color-accent)]"
            >
              <span className="accent">$</span> c0mpute
            </Link>
            <div className="flex gap-5 text-[var(--color-dim)]">
              <Link href="/getting-started" className="!border-0 hover:text-[var(--color-accent)]">getting-started</Link>
              <Link href="/plugins" className="!border-0 hover:text-[var(--color-accent)]">plugins</Link>
              <Link href="/docs" className="!border-0 hover:text-[var(--color-accent)]">docs</Link>
              <Link href="/blog" className="!border-0 hover:text-[var(--color-accent)]">blog</Link>
              <Link href="/status" className="!border-0 hover:text-[var(--color-accent)]">status</Link>
              <Link href="/contact" className="!border-0 hover:text-[var(--color-accent)]">contact</Link>
            </div>
          </nav>
        </header>

        <main className="flex-1">{children}</main>

        <footer className="border-t border-[var(--color-rule)] mt-16">
          <div className="max-w-3xl mx-auto px-6 py-6 flex flex-wrap gap-x-6 gap-y-2 text-xs text-[var(--color-dim)]">
            <span>c0mpute.com</span>
            <Link href="/blog" className="!border-0 hover:text-[var(--color-accent)]">blog</Link>
            <Link href="/about" className="!border-0 hover:text-[var(--color-accent)]">about</Link>
            <Link href="/pricing" className="!border-0 hover:text-[var(--color-accent)]">pricing</Link>
            <Link href="/terms" className="!border-0 hover:text-[var(--color-accent)]">terms</Link>
            <Link href="/privacy" className="!border-0 hover:text-[var(--color-accent)]">privacy</Link>
            <a
              href="https://github.com/profullstack/c0mpute"
              className="!border-0 hover:text-[var(--color-accent)]"
            >
              github
            </a>
            <span className="ml-auto">MIT</span>
          </div>
        </footer>
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
      <FeedbackWidget property="c0mpute.com" />
      </body>
    </html>
  );
}
