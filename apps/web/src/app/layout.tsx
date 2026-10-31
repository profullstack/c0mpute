import PwaRegistration from "./PwaRegistration";
import type { Metadata, Viewport } from "next";
import "./globals.css";
import Script from "next/script";
import { Footer } from "@profullstack/footer/react";
import { WebMcpProvider } from "@/components/webmcp-provider";
import { SiteNav } from "@/components/site-nav";

const SITE = "https://c0mpute.com";
const DESCRIPTION =
  "The open compute network. Run jobs anywhere, sell unused compute, pay only for verified work. CPUs, GPUs, storage and bandwidth from PCs to data centers, through one open protocol — for humans and AI agents.";

// Static pages re-render hourly so they pick up the shared footer template
// without waiting for a redeploy.
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
        <PwaRegistration />
        <WebMcpProvider />
        <header className="border-b border-[var(--color-rule)]">
          <SiteNav />
        </header>

        <main className="flex-1">{children}</main>

        {/* The shared template carries the site links, webring, and copyright. */}
        <div className="text-xs text-[var(--color-dim)] [&_a]:!border-0">
          <Footer
            site="https://c0mpute.com/"
            tagline="MIT licensed"
            links={[
              { label: "Blog", href: "/blog" },
              { label: "About", href: "/about" },
              { label: "Contact", href: "/contact" },
              { label: "Market", href: "/market" },
              { label: "Pricing", href: "/pricing" },
              { label: "Terms", href: "/terms" },
              { label: "Privacy", href: "/privacy" },
              { label: "GitHub", href: "https://github.com/profullstack/c0mpute", rel: "noopener noreferrer" },
            ]}
          />
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
      </body>
    </html>
  );
}
