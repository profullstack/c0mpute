import type { Metadata, Viewport } from "next";
import Link from "next/link";
import "./globals.css";

export const metadata: Metadata = {
  title: "c0mpute — decentralized compute network",
  description:
    "Decentralized compute marketplace. CLI-first. Three modules: transcode (FFmpeg), coinpay (DID + payments), infernet (AI inference).",
  manifest: "/manifest.json",
  appleWebApp: {
    capable: true,
    statusBarStyle: "default",
    title: "c0mpute",
  },
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
              <Link href="/status" className="!border-0 hover:text-[var(--color-accent)]">status</Link>
              <Link href="/contact" className="!border-0 hover:text-[var(--color-accent)]">contact</Link>
            </div>
          </nav>
        </header>

        <main className="flex-1">{children}</main>

        <footer className="border-t border-[var(--color-rule)] mt-16">
          <div className="max-w-3xl mx-auto px-6 py-6 flex flex-wrap gap-x-6 gap-y-2 text-xs text-[var(--color-dim)]">
            <span>c0mpute.com</span>
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
      </body>
    </html>
  );
}
