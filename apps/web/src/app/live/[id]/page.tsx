import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { liveStore, siteUrl } from "@/lib/chat/server";
import type { LiveStream } from "@/lib/live/store";
import { LivePlayer } from "./player";

// A live stream, watched by anyone with the link: no sign-in, no app. The
// video comes from c0mpute nodes relaying it to each other (DIP-0019).

export const dynamic = "force-dynamic";

async function load(id: string): Promise<LiveStream | null> {
  return liveStore()
    .publicGet(id)
    .catch(() => null);
}

export async function generateMetadata({ params }: { params: Promise<{ id: string }> }): Promise<Metadata> {
  const s = await load((await params).id);
  if (!s) return { title: "Live stream — c0mpute", robots: { index: false } };
  const url = `${siteUrl()}/live/${s.id}`;
  const description = `${s.live ? "Live now" : "Off air"} on c0mpute Live: streamed peer to peer by c0mpute nodes.`;
  return {
    title: `${s.title} — c0mpute Live`,
    description,
    robots: s.visibility === "public" ? undefined : { index: false, follow: false },
    alternates: { canonical: url },
    openGraph: { type: "video.other", siteName: "c0mpute", title: s.title, description, url },
  };
}

export default async function LivePage({ params }: { params: Promise<{ id: string }> }) {
  const s = await load((await params).id);
  if (!s) notFound();
  const playlist = `${siteUrl()}/live/${s.id}/index.m3u8`;
  return (
    <main className="max-w-3xl mx-auto px-4 sm:px-6 py-8">
      <p className="text-[15px] text-[var(--color-dim)]">c0mpute Live · {s.live ? "live now" : "off air"}</p>
      <h1 className="mt-2 mb-4 text-[26px] leading-tight font-bold text-[var(--color-fg)]">{s.title}</h1>
      <LivePlayer src={playlist} live={s.live} />
      <p className="mt-6 text-[15px] break-all">
        Playlist for VLC, mpv or ffplay: <a href={playlist}>{playlist}</a>
      </p>
      <p className="mt-6 text-[15px] text-[var(--color-dim)]">
        This stream is carried by c0mpute nodes, which are paid per GB they relay.{" "}
        <Link href="/live">Stream your own</Link> or <Link href="/getting-started">run a node and earn from streams like this one</Link>.
      </p>
    </main>
  );
}
