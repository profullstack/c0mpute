// The pit.world network footer, fetched on the server and rendered as the last
// thing in <body>. Cached for an hour; on any failure (timeout, non-200, network)
// it renders nothing so the page never breaks.
const PIT_FOOTER_URL = "https://pit.world/snippets/footer.html?site=c0mpute.com";

export async function PitFooter() {
  try {
    const res = await fetch(PIT_FOOTER_URL, {
      next: { revalidate: 3600 },
      signal: AbortSignal.timeout(3000),
    });
    if (!res.ok) return null;
    // One line: the snippet's newlines are cosmetic, and keeping it on a single
    // line keeps the page's markup tidy alongside the hydration payload.
    const html = (await res.text()).replace(/\s*\n\s*/g, " ").trim();
    if (!html.includes("data-pit-footer")) return null;
    return <div dangerouslySetInnerHTML={{ __html: html }} />;
  } catch {
    return null;
  }
}
