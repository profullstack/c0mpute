import Script from "next/script";

/**
 * Every blog page carries one CrawlProof unit under the post: the text strip,
 * filled by ad.js, which collapses to nothing when there is no advertiser.
 * The tracker is in the root layout; this is the ads half of the blog's
 * defaults (tracking + ads, both on).
 */
export default function BlogLayout({ children }: { children: React.ReactNode }) {
  return (
    <>
      {children}
      <aside data-cp-ad="" data-slot="f68457c4-7402-4773-ab8f-8c802238d85a" data-format="text_link" />
      <Script src="https://crawlproof.com/ad.js" strategy="afterInteractive" />
    </>
  );
}
