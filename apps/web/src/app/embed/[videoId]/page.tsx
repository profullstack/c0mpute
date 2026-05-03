/**
 * Embed iframe target. Hosted at /video/embed/<videoId>.
 *
 * The actual HLS player is wired in M1 — this page renders the layout +
 * placeholder so partners can already iframe the URL.
 */

import "./embed.css";

interface Props {
  params: Promise<{ videoId: string }>;
}

export default async function EmbedPage({ params }: Props) {
  const { videoId } = await params;
  return (
    <div className="embed-root">
      <div className="embed-stage">
        <p className="embed-placeholder">
          Quest player loading <code>{videoId}</code>…
        </p>
      </div>
    </div>
  );
}

export const metadata = {
  title: "Quest player",
  robots: { index: false, follow: false },
};
